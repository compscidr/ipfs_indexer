//! Token-protected admin endpoints for darksearch.xyz's admin area: edit the local takedown
//! list, and read and change the settings (#304). Registered only when ADMIN_TOKEN is set;
//! without it the routes don't exist (404).
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use actix_web::dev::ServerHandle;
use actix_web::{delete, get, post, web, HttpRequest, HttpResponse};
use cid::Cid;
use log::{info, warn};
use serde_json::json;

use crate::denylist::{normalize_item, Denylist};
use crate::index_queue::IndexQueue;
use crate::settings::{Applies, Key};

pub struct Admin {
    pub token: String,
    pub local_denylist: Option<PathBuf>,
    pub restart: web::Data<Restart>,
}

/// The running server, so a restart setting can stop it; main sets it once the server is built
#[derive(Default)]
pub struct Restart(OnceLock<ServerHandle>);

impl Restart {
    pub fn set(&self, handle: ServerHandle) {
        let _ = self.0.set(handle);
    }

    // stops the server a second after the reply has gone out; main then flushes the index and
    // exits 0, and Docker (unless-stopped) starts it again with the new value
    fn after_reply(&self, name: &str) {
        info!("Restarting to apply {}", name);
        // unset only in tests, which have no server to stop
        if let Some(handle) = self.0.get().cloned() {
            run_after(Duration::from_secs(1), move || async move {
                handle.stop(true).await;
            });
        }
    }
}

// runs `f` after `delay` on the actix runtime, without holding up the caller
fn run_after<F, Fut>(delay: Duration, f: F)
where
    F: FnOnce() -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    actix_web::rt::spawn(async move {
        actix_web::rt::time::sleep(delay).await;
        f().await;
    });
}

// serializes edits of the local denylist file with each other and with the periodic re-read
static LOCAL_DENYLIST_FILE: Mutex<()> = Mutex::new(());

// the lock guards no data, so a panic while holding it mustn't stop takedowns for good
fn file_lock() -> std::sync::MutexGuard<'static, ()> {
    LOCAL_DENYLIST_FILE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Re-read the local denylist; the purge thread erases what it newly blocks
pub fn reload_local(queue: &IndexQueue, path: &Path) -> std::io::Result<()> {
    let _file = file_lock();
    apply(queue, path)
}

fn apply(queue: &IndexQueue, path: &Path) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    queue.set_local_denylist(Denylist::parse(&text));
    Ok(())
}

pub fn configure(cfg: &mut web::ServiceConfig, admin: Option<web::Data<Admin>>) {
    if let Some(admin) = admin {
        cfg.app_data(admin)
            .service(deny)
            .service(undeny)
            .service(recrawl)
            .service(settings_read)
            .service(setting_set)
            .service(setting_reset);
    }
}

fn authorized(req: &HttpRequest, admin: &Admin) -> bool {
    let given = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    constant_time_eq(given.as_bytes(), admin.token.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// a "cid", "cid/path" or "/ipfs/cid/path" line as its canonical item; None for comments,
// "//hash" lines and anything else
fn entry_item(line: &str) -> Option<String> {
    let line = line.trim();
    let item = normalize_item(line.strip_prefix("/ipfs/").unwrap_or(line))?;
    Cid::try_from(item.split('/').next()?).ok()?;
    Some(item)
}

// checks the request, rewrites the local denylist with `edit(text, item)` and applies it
fn edit_local(
    req: &HttpRequest,
    body: &str,
    queue: &IndexQueue,
    admin: &Admin,
    edit: impl FnOnce(&str, &str) -> String,
    describe: impl FnOnce(&str, &str) -> String,
) -> HttpResponse {
    if !authorized(req, admin) {
        return HttpResponse::Unauthorized().body("Unauthorized");
    }
    let Some(item) = entry_item(body) else {
        return HttpResponse::BadRequest().body("Not a valid cid or cid/path");
    };
    let Some(path) = &admin.local_denylist else {
        return HttpResponse::BadRequest().body("LOCAL_DENYLIST is not set");
    };
    // ponytail: file IO runs on the actix worker; fine for occasional admin calls
    let _file = file_lock();
    let result = std::fs::read_to_string(path).and_then(|old| {
        let new = edit(&old, &item);
        write_replacing(path, &new)?;
        apply(queue, path)?;
        Ok(describe(&old, &new))
    });
    match result {
        Ok(message) => HttpResponse::Ok().body(message),
        Err(err) => {
            warn!("Couldn't update {}: {}", path.display(), err);
            HttpResponse::InternalServerError().body("Couldn't update the local denylist")
        }
    }
}

#[post("/admin/recrawl")]
async fn recrawl(
    req: HttpRequest,
    queue: web::Data<IndexQueue>,
    admin: web::Data<Admin>,
) -> HttpResponse {
    if !authorized(&req, &admin) {
        return HttpResponse::Unauthorized().body("Unauthorized");
    }
    let queued = queue.recrawl();
    HttpResponse::Ok().body(format!(
        "Queued {} to crawl again",
        plural(queued, "page", "pages")
    ))
}

// whole-file replace through a temp file in the same directory: never half a list on disk
fn write_replacing(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[post("/admin/deny")]
async fn deny(
    req: HttpRequest,
    body: String,
    queue: web::Data<IndexQueue>,
    admin: web::Data<Admin>,
) -> HttpResponse {
    edit_local(
        &req,
        &body,
        &queue,
        &admin,
        |text, item| {
            let sep = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            format!("{}{}{}\n", text, sep, item)
        },
        |_, _| "Blocked; removal from disk queued".to_string(),
    )
}

#[post("/admin/undeny")]
async fn undeny(
    req: HttpRequest,
    body: String,
    queue: web::Data<IndexQueue>,
    admin: web::Data<Admin>,
) -> HttpResponse {
    edit_local(
        &req,
        &body,
        &queue,
        &admin,
        |text, item| {
            text.lines()
                .filter(|line| entry_item(line).as_deref() != Some(item))
                .map(|line| format!("{}\n", line))
                .collect()
        },
        |old, new| {
            // shrinking the list never purges pages, so report the lines that were taken out
            let lines = old.lines().count() - new.lines().count();
            format!(
                "Unblocked; {}",
                plural(lines, "line removed", "lines removed")
            )
        },
    )
}

#[get("/admin/settings")]
async fn settings_read(
    req: HttpRequest,
    queue: web::Data<IndexQueue>,
    admin: web::Data<Admin>,
) -> HttpResponse {
    if !authorized(&req, &admin) {
        return HttpResponse::Unauthorized().body("Unauthorized");
    }
    // the report shows the secret only as "set" or "not set"
    HttpResponse::Ok().json(json!({
        "settings": queue.settings().report(),
        "persistent": queue.has_persistent_store(),
    }))
}

#[post("/admin/settings/{name}")]
async fn setting_set(
    req: HttpRequest,
    name: web::Path<String>,
    body: String,
    queue: web::Data<IndexQueue>,
    admin: web::Data<Admin>,
) -> HttpResponse {
    // a trailing newline from curl or a form isn't part of the value
    change_setting(&req, &name, &queue, &admin, |key, actor| {
        queue.settings_set(key, body.trim(), actor)
    })
}

#[delete("/admin/settings/{name}")]
async fn setting_reset(
    req: HttpRequest,
    name: web::Path<String>,
    queue: web::Data<IndexQueue>,
    admin: web::Data<Admin>,
) -> HttpResponse {
    change_setting(&req, &name, &queue, &admin, |key, actor| {
        queue.settings_reset(key, actor)
    })
}

// checks a set or reset request, makes the change with `change(key, actor)`, and restarts if
// the setting is only read at startup. Settings::set/reset do the logging, so the secret has
// one code path; their errors never contain it.
fn change_setting(
    req: &HttpRequest,
    name: &str,
    queue: &IndexQueue,
    admin: &Admin,
    change: impl FnOnce(Key, &str) -> Result<Applies, String>,
) -> HttpResponse {
    if !authorized(req, admin) {
        return HttpResponse::Unauthorized().body("Unauthorized");
    }
    let Some(key) = Key::from_name(name) else {
        return HttpResponse::NotFound().body("No such setting");
    };
    let actor = req
        .headers()
        .get("X-Admin-Actor")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|actor| !actor.is_empty());
    let Some(actor) = actor else {
        return HttpResponse::BadRequest().body("X-Admin-Actor is required");
    };
    // the in-memory store would lose the change at the next start
    if !queue.has_persistent_store() {
        return HttpResponse::BadRequest().body("settings need DATA_DIR");
    }
    match change(key, actor) {
        Ok(applies) => {
            if applies == Applies::Restart {
                admin.restart.after_reply(key.name());
            }
            HttpResponse::Ok().json(json!({ "applies": applies.label() }))
        }
        Err(reason) => HttpResponse::BadRequest().body(reason),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", n, if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use actix_web::{test, web, App};

    use super::*;
    use crate::index_queue::Enqueued;
    use crate::index_result::IndexResult;

    const GOOD: &str = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
    const BAD: &str = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";

    fn admin_with(list: &str) -> (web::Data<Admin>, PathBuf) {
        // temp_dir names by pid + nanos, which tests running in parallel can share
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = crate::store::tests::temp_dir(&format!("admin{}", n)).join("local.deny");
        std::fs::write(&path, list).unwrap();
        let admin = Admin {
            token: "s3cret".to_string(),
            local_denylist: Some(path.clone()),
            restart: web::Data::new(Restart::default()),
        };
        (web::Data::new(admin), path)
    }

    async fn call(
        admin: Option<web::Data<Admin>>,
        queue: web::Data<IndexQueue>,
        uri: &str,
        token: &str,
        body: &str,
    ) -> (u16, String) {
        let req = test::TestRequest::post()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_payload(body.to_string());
        send(admin, queue, req).await
    }

    async fn send(
        admin: Option<web::Data<Admin>>,
        queue: web::Data<IndexQueue>,
        req: test::TestRequest,
    ) -> (u16, String) {
        let app = test::init_service(
            App::new()
                .app_data(queue)
                .configure(|cfg| configure(cfg, admin)),
        )
        .await;
        let req = req.to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status().as_u16();
        (
            status,
            String::from_utf8(test::read_body(resp).await.to_vec()).unwrap(),
        )
    }

    #[actix_web::test]
    async fn a_panic_while_holding_the_file_lock_doesnt_stop_reloads() {
        let (_, path) = admin_with(&format!("{}\n", BAD));
        let _ = std::thread::spawn(|| {
            let _file = LOCAL_DENYLIST_FILE.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        let queue = IndexQueue::new();
        reload_local(&queue, &path).unwrap();
        assert_eq!(queue.enqueue(BAD.to_string()), Enqueued::Blocked);
    }

    #[actix_web::test]
    async fn without_admin_token_the_routes_dont_exist() {
        let queue = web::Data::new(IndexQueue::new());
        assert_eq!(
            call(None, queue, "/admin/deny", "s3cret", GOOD).await.0,
            404
        );
    }

    #[actix_web::test]
    async fn a_wrong_or_missing_token_is_401_and_changes_nothing() {
        let (admin, path) = admin_with("# list\n");
        let queue = web::Data::new(IndexQueue::new());
        assert_eq!(
            call(
                Some(admin.clone()),
                queue.clone(),
                "/admin/deny",
                "nope",
                GOOD
            )
            .await
            .0,
            401
        );
        assert_eq!(
            call(Some(admin), queue, "/admin/deny", "", GOOD).await.0,
            401
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# list\n");
    }

    #[actix_web::test]
    async fn deny_appends_purges_and_survives_the_reread() {
        let (admin, path) = admin_with("# takedowns"); // no trailing newline
        let queue = web::Data::new(IndexQueue::new());
        let item = format!("{}/x", BAD);
        queue.index_for_test(
            &item,
            &IndexResult::new(item.clone(), "t".into(), "e".into(), HashMap::new()),
        );

        let (status, body) = call(
            Some(admin),
            queue.clone(),
            "/admin/deny",
            "s3cret",
            &format!("/ipfs/{}/x\n", BAD),
        )
        .await;
        assert_eq!(
            (status, body.as_str()),
            (200, "Blocked; removal from disk queued")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("# takedowns\n{}\n", item)
        );
        queue.purge_for_test();
        assert_eq!(queue.index_length(), 0);

        // the 5-minute re-read takes the same lock and keeps it blocked
        reload_local(&queue, &path).unwrap();
        assert_eq!(queue.enqueue(item), Enqueued::Blocked);
    }

    #[actix_web::test]
    async fn undeny_removes_only_matching_lines() {
        let list = format!(
            "# keep\n{g}\n/ipfs/{b}/x\n{b}/y\n//abcd\n",
            g = GOOD,
            b = BAD
        );
        let (admin, path) = admin_with(&list);
        let queue = web::Data::new(IndexQueue::new());
        let (status, body) = call(
            Some(admin),
            queue.clone(),
            "/admin/undeny",
            "s3cret",
            &format!("{}/x/", BAD),
        )
        .await;
        assert_eq!((status, body.as_str()), (200, "Unblocked; 1 line removed"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("# keep\n{g}\n{b}/y\n//abcd\n", g = GOOD, b = BAD)
        );
        assert_eq!(queue.enqueue(format!("{}/x", BAD)), Enqueued::Queued);
    }

    #[actix_web::test]
    async fn recrawl_queues_every_indexed_page_once() {
        let (admin, _) = admin_with("");
        let queue = web::Data::new(IndexQueue::new());
        for item in [GOOD.to_string(), format!("{}/wiki/X", GOOD)] {
            queue.index_for_test(
                &item,
                &IndexResult::new(item.clone(), "t".into(), "e".into(), HashMap::new()),
            );
        }
        let post = |token: &'static str| {
            call(
                Some(admin.clone()),
                queue.clone(),
                "/admin/recrawl",
                token,
                "",
            )
        };
        assert_eq!(post("nope").await.0, 401);
        assert_eq!(queue.queue_length(), 0);
        assert_eq!(
            post("s3cret").await,
            (200, "Queued 2 pages to crawl again".to_string())
        );
        assert_eq!(queue.queue_length(), 2);
        // already queued: not queued twice
        assert_eq!(
            post("s3cret").await,
            (200, "Queued 0 pages to crawl again".to_string())
        );
        assert_eq!(
            queue.index_length(),
            2,
            "pages stay searchable while they wait"
        );
    }

    #[actix_web::test]
    async fn invalid_items_are_400_and_change_nothing() {
        let (admin, path) = admin_with("");
        let queue = web::Data::new(IndexQueue::new());
        for body in ["notacid", &format!("{}/../..", BAD), ""] {
            let (status, _) = call(
                Some(admin.clone()),
                queue.clone(),
                "/admin/deny",
                "s3cret",
                body,
            )
            .await;
            assert_eq!(status, 400, "{:?}", body);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[actix_web::test]
    async fn deny_without_a_local_denylist_is_400() {
        let admin = web::Data::new(Admin {
            token: "s3cret".to_string(),
            local_denylist: None,
            restart: web::Data::new(Restart::default()),
        });
        let queue = web::Data::new(IndexQueue::new());
        assert_eq!(
            call(Some(admin), queue, "/admin/deny", "s3cret", GOOD)
                .await
                .0,
            400
        );
    }

    // the settings endpoints (#304)

    fn settings_admin() -> web::Data<Admin> {
        web::Data::new(Admin {
            token: "s3cret".to_string(),
            local_denylist: None,
            restart: web::Data::new(Restart::default()),
        })
    }

    // a queue with a store from a DATA_DIR, so settings can be saved
    fn persistent_queue(name: &str) -> web::Data<IndexQueue> {
        let queue = IndexQueue::new();
        let dir = crate::store::tests::temp_dir(name);
        queue
            .attach_store(crate::store::Store::open(&dir, 0).unwrap())
            .unwrap();
        web::Data::new(queue)
    }

    // a settings request with the token, and the actor when given
    fn settings_req(
        method: &str,
        name: &str,
        actor: Option<&str>,
        body: &str,
    ) -> test::TestRequest {
        let uri = match name {
            "" => "/admin/settings".to_string(),
            name => format!("/admin/settings/{}", name),
        };
        let req = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            _ => test::TestRequest::delete(),
        }
        .uri(&uri)
        .insert_header(("Authorization", "Bearer s3cret"))
        .set_payload(body.to_string());
        match actor {
            Some(actor) => req.insert_header(("X-Admin-Actor", actor)),
            None => req,
        }
    }

    async fn read_settings(queue: &web::Data<IndexQueue>) -> serde_json::Value {
        let (status, body) = send(
            Some(settings_admin()),
            queue.clone(),
            settings_req("GET", "", None, ""),
        )
        .await;
        assert_eq!(status, 200, "{}", body);
        serde_json::from_str(&body).unwrap()
    }

    fn row<'a>(read: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        read["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap()
    }

    #[actix_web::test]
    async fn settings_read_lists_every_key_with_sources() {
        let queue = web::Data::new(IndexQueue::new());
        let read = read_settings(&queue).await;
        assert_eq!(read["settings"].as_array().unwrap().len(), 18);
        assert_eq!(row(&read, "crawl_workers")["source"], "default");
        assert_eq!(read["persistent"], false);
        assert_eq!(
            read_settings(&persistent_queue("settings-read")).await["persistent"],
            true
        );
    }

    #[actix_web::test]
    async fn setting_a_value_needs_an_actor_and_a_valid_value() {
        let queue = persistent_queue("settings-set");
        let post = |actor: Option<&'static str>, body: &'static str| {
            send(
                Some(settings_admin()),
                queue.clone(),
                settings_req("POST", "crawl_workers", actor, body),
            )
        };
        assert_eq!(
            post(None, "25").await,
            (400, "X-Admin-Actor is required".to_string())
        );
        assert_eq!(post(Some(" "), "25").await.0, 400, "an empty actor");
        let (status, body) = post(Some("ann@x"), "abc").await;
        assert_eq!(status, 400);
        assert!(
            body.contains("crawl_workers") || body.contains("CRAWL_WORKERS"),
            "{}",
            body
        );
        assert_eq!(
            row(&read_settings(&queue).await, "crawl_workers")["source"],
            "default"
        );

        assert_eq!(
            post(Some("ann@x"), "25\n").await,
            (200, r#"{"applies":"live"}"#.to_string())
        );
        let read = read_settings(&queue).await;
        let workers = row(&read, "crawl_workers");
        assert_eq!(workers["value"], 25);
        assert_eq!(workers["source"], "admin");
        assert_eq!(workers["actor"], "ann@x");
    }

    #[actix_web::test]
    async fn resetting_falls_back() {
        let queue = persistent_queue("settings-reset");
        let call = |method: &'static str, actor: Option<&'static str>| {
            send(
                Some(settings_admin()),
                queue.clone(),
                settings_req(method, "crawl_workers", actor, "25"),
            )
        };
        assert_eq!(call("POST", Some("ann@x")).await.0, 200);
        assert_eq!(
            call("DELETE", None).await,
            (400, "X-Admin-Actor is required".to_string())
        );
        assert_eq!(
            call("DELETE", Some("ann@x")).await,
            (200, r#"{"applies":"live"}"#.to_string())
        );
        let read = read_settings(&queue).await;
        let workers = row(&read, "crawl_workers");
        assert_eq!(workers["value"], 10);
        assert_eq!(workers["source"], "default");
    }

    #[actix_web::test]
    async fn an_unknown_setting_is_404() {
        let queue = persistent_queue("settings-unknown");
        for method in ["POST", "DELETE"] {
            let (status, _) = send(
                Some(settings_admin()),
                queue.clone(),
                settings_req(method, "data_dir", Some("ann@x"), "/tmp"),
            )
            .await;
            assert_eq!(status, 404, "{}", method);
        }
    }

    #[actix_web::test]
    async fn the_secret_never_comes_back() {
        let queue = persistent_queue("settings-secret");
        let (status, body) = send(
            Some(settings_admin()),
            queue.clone(),
            settings_req(
                "POST",
                "eth_rpc_url",
                Some("ann@x"),
                "https://x.example/v2/SECRETKEY",
            ),
        )
        .await;
        assert_eq!((status, body.as_str()), (200, r#"{"applies":"restart"}"#));
        let read = read_settings(&queue).await;
        assert!(!read.to_string().contains("SECRETKEY"), "{}", read);
        assert_eq!(row(&read, "eth_rpc_url")["value"], "set");

        // a bad value isn't echoed either
        let (status, body) = send(
            Some(settings_admin()),
            queue.clone(),
            settings_req("POST", "eth_rpc_url", Some("ann@x"), "ftp://SECRETKEY"),
        )
        .await;
        assert_eq!(status, 400);
        assert!(!body.contains("SECRETKEY"), "{}", body);
    }

    #[actix_web::test]
    async fn a_restart_setting_answers_before_stopping() {
        // no server handle in tests: the restart is skipped, the answer still comes back
        let queue = persistent_queue("settings-restart");
        let (status, body) = send(
            Some(settings_admin()),
            queue.clone(),
            settings_req("POST", "eth_logs_max_blocks", Some("ann@x"), "50"),
        )
        .await;
        assert_eq!((status, body.as_str()), (200, r#"{"applies":"restart"}"#));
        assert_eq!(
            row(&read_settings(&queue).await, "eth_logs_max_blocks")["value"],
            50
        );
    }

    #[actix_web::test]
    async fn a_scheduled_stop_runs_only_after_the_delay() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        run_after(Duration::from_millis(200), move || async move {
            flag.store(true, Ordering::SeqCst);
        });
        actix_web::rt::time::sleep(Duration::from_millis(50)).await;
        assert!(!stopped.load(Ordering::SeqCst), "ran before the delay");
        actix_web::rt::time::sleep(Duration::from_millis(500)).await;
        assert!(stopped.load(Ordering::SeqCst), "never ran");
    }

    #[actix_web::test]
    async fn without_data_dir_writes_are_refused() {
        let queue = web::Data::new(IndexQueue::new());
        for method in ["POST", "DELETE"] {
            assert_eq!(
                send(
                    Some(settings_admin()),
                    queue.clone(),
                    settings_req(method, "crawl_workers", Some("ann@x"), "25"),
                )
                .await,
                (400, "settings need DATA_DIR".to_string()),
                "{}",
                method
            );
        }
        assert_eq!(
            row(&read_settings(&queue).await, "crawl_workers")["value"],
            10
        );
        assert!(
            !queue.has_persistent_store(),
            "the in-memory store doesn't count"
        );
    }

    #[actix_web::test]
    async fn without_the_token_settings_routes_are_401() {
        let queue = persistent_queue("settings-401");
        for method in ["GET", "POST", "DELETE"] {
            let name = if method == "GET" { "" } else { "crawl_workers" };
            let req = settings_req(method, name, Some("ann@x"), "25")
                .insert_header(("Authorization", "Bearer nope"));
            let (status, _) = send(Some(settings_admin()), queue.clone(), req).await;
            assert_eq!(status, 401, "{}", method);
            let req = settings_req(method, name, Some("ann@x"), "25");
            let (status, _) = send(None, queue.clone(), req).await;
            assert_eq!(status, 404, "{} without ADMIN_TOKEN", method);
        }
        assert_eq!(
            row(&read_settings(&queue).await, "crawl_workers")["source"],
            "default"
        );
    }
}
