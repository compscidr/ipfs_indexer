//! Token-protected admin endpoints for darksearch.xyz's admin area: edit the local takedown
//! list. Registered only when ADMIN_TOKEN is set; without it the routes don't exist (404).
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use actix_web::{post, web, HttpRequest, HttpResponse};
use cid::Cid;
use log::warn;

use crate::denylist::{normalize_item, Denylist};
use crate::index_queue::IndexQueue;

pub struct Admin {
    pub token: String,
    pub local_denylist: Option<PathBuf>,
}

// serializes edits of the local denylist file with each other and with the periodic re-read
static LOCAL_DENYLIST_FILE: Mutex<()> = Mutex::new(());

// the lock guards no data, so a panic while holding it mustn't stop takedowns for good
fn file_lock() -> std::sync::MutexGuard<'static, ()> {
    LOCAL_DENYLIST_FILE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Re-read the local denylist and purge what it blocks; returns how many pages were removed
pub fn reload_local(queue: &IndexQueue, path: &Path) -> std::io::Result<usize> {
    let _file = file_lock();
    apply(queue, path)
}

fn apply(queue: &IndexQueue, path: &Path) -> std::io::Result<usize> {
    let text = std::fs::read_to_string(path)?;
    Ok(queue.set_local_denylist(Denylist::parse(&text)))
}

pub fn configure(cfg: &mut web::ServiceConfig, admin: Option<web::Data<Admin>>) {
    if let Some(admin) = admin {
        cfg.app_data(admin).service(deny).service(undeny);
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
    describe: impl FnOnce(&str, &str, usize) -> String,
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
    // ponytail: file IO and the purge run on the actix worker; fine for occasional admin calls
    let _file = file_lock();
    let result = std::fs::read_to_string(path).and_then(|old| {
        let new = edit(&old, &item);
        write_replacing(path, &new)?;
        Ok(describe(&old, &new, apply(queue, path)?))
    });
    match result {
        Ok(message) => HttpResponse::Ok().body(message),
        Err(err) => {
            warn!("Couldn't update {}: {}", path.display(), err);
            HttpResponse::InternalServerError().body("Couldn't update the local denylist")
        }
    }
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
        |_, _, pages| {
            format!(
                "Blocked; {}",
                plural(pages, "page removed", "pages removed")
            )
        },
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
        |old, new, _| {
            // shrinking the list never purges pages, so report the lines that were taken out
            let lines = old.lines().count() - new.lines().count();
            format!(
                "Unblocked; {}",
                plural(lines, "line removed", "lines removed")
            )
        },
    )
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
        let app = test::init_service(
            App::new()
                .app_data(queue)
                .configure(|cfg| configure(cfg, admin)),
        )
        .await;
        let req = test::TestRequest::post()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_payload(body.to_string())
            .to_request();
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
        queue.map.insert(
            item.clone(),
            IndexResult::new(item.clone(), "t".into(), "e".into(), HashMap::new()),
        );

        let (status, body) = call(
            Some(admin),
            queue.clone(),
            "/admin/deny",
            "s3cret",
            &format!("/ipfs/{}/x\n", BAD),
        )
        .await;
        assert_eq!((status, body.as_str()), (200, "Blocked; 1 page removed"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("# takedowns\n{}\n", item)
        );
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
        });
        let queue = web::Data::new(IndexQueue::new());
        assert_eq!(
            call(Some(admin), queue, "/admin/deny", "s3cret", GOOD)
                .await
                .0,
            400
        );
    }
}
