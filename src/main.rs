use actix_web::{
    App, HttpRequest, HttpResponse, HttpServer, Responder, get, put,
    rt::{self, spawn, time::Instant},
    web::{self, Bytes, Data},
};

const INDEX_HTML: &str = include_str!("index.html");
use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use async_lock::Semaphore;
use clap::Parser;
use metrics::{counter, gauge};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use rand::{RngExt, seq::IteratorRandom};
use scc::HashMap;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, str::FromStr, sync::LazyLock, time::Duration};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

/// Global concurrency limiter; caps simultaneous in-flight requests to `max_slots`.
static LIMITER: LazyLock<Semaphore> = LazyLock::new(|| {
    let config = Config::try_parse().unwrap_or_default();
    Semaphore::new(config.max_slots)
});

/// Minimum byte size of the random dummy payload returned on a cache miss.
const MIN_DUMMY_OBJECT_SIZE: usize = 420_069;
/// Maximum byte size of the random dummy payload returned on a cache miss.
const MAX_DUMMY_OBJECT_SIZE: usize = 1_000_000;

/// Server configuration, populated from CLI arguments.
#[derive(Clone, Debug, Serialize, Deserialize, Parser)]
#[command(version, about, long_about = None)]
struct Config {
    /// Address and port to listen on (e.g. `127.0.0.1:8420`).
    #[arg(short = 'L', long, default_value_t = Self::default().listen)]
    listen: String,

    /// Maximum size in bytes of a single stored object.
    #[arg(short = 'o', long, default_value_t = Self::default().max_object_size)]
    max_object_size: u64,

    /// Maximum number of objects that may be held in storage at once.
    #[arg(short = 'm', long, default_value_t = Self::default().max_objects)]
    max_objects: u64,

    /// Maximum number of concurrent requests allowed at any moment.
    #[arg(short = 's', long, default_value_t = Self::default().max_slots)]
    max_slots: usize,

    /// Maximum age of a stored object in seconds before it is pruned.
    #[arg(short = 'a', long, default_value_t = Self::default().max_age_secs)]
    max_age_secs: u64,

    /// Interval in seconds between pruner runs.
    #[arg(short = 'p', long, default_value_t = Self::default().prune_secs)]
    prune_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8420".to_string(),
            max_object_size: 1_000_000, // 1MB
            max_objects: 256,
            max_slots: 10,
            max_age_secs: 60 * 60, // 1 hour
            prune_secs: 60,
        }
    }
}

/// An object held in the in-memory store, waiting to be taken by a consumer.
struct StoredObject {
    /// Timestamp of when the object was stored, used to enforce `max_age_secs`.
    created: Instant,

    /// The `Content-Type` value supplied by the uploader.
    content_type: String,

    /// AES-256-GCM nonce used when encrypting `bytes`. Must be supplied for decryption.
    nonce: [u8; 12],

    /// AES-256-GCM ciphertext of the original body. The key is never stored here.
    bytes: Bytes,
}

impl StoredObject {
    /// Returns `true` if the object has been in storage longer than `max_age` seconds.
    fn is_too_old(&self, max_age: u64) -> bool {
        self.created.elapsed().as_secs() >= max_age
    }
}

/// Generate a random dummy payload of printable ASCII characters.
fn generate_dummy_object() -> Bytes {
    let mut rng = rand::rng();
    let size = (MIN_DUMMY_OBJECT_SIZE..=MAX_DUMMY_OBJECT_SIZE)
        .choose(&mut rng)
        .expect("aww crap");
    let vec: Vec<u8> = (0..size).map(|_| rng.random_range(b' '..=b'~')).collect();
    Bytes::from(vec)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let config = Config::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let metrics_handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let metrics_handle = Data::new(metrics_handle);

    let bind = config
        .listen
        .parse::<SocketAddr>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, e.to_string()))?;

    let storage = scc::HashMap::<Uuid, StoredObject>::new();
    let owned_storage = Data::new(storage);

    // Start a pruner thread to periodically wipe out old entries
    let pruner_storage = owned_storage.clone();
    let pruner_handle = metrics_handle.clone();
    spawn(async move {
        let config = Config::parse();
        let storage = pruner_storage;
        let metrics_handle = pruner_handle;

        let ticker = rt::time::interval(Duration::from_secs(config.prune_secs));
        rt::pin!(ticker);
        ticker.tick().await; // first tick is instant
        loop {
            let _ = ticker.tick().await;
            storage
                .retain_async(|_id, object| !object.is_too_old(config.max_age_secs))
                .await;

            counter!("pruner_runs").increment(1);
            metrics_handle.run_upkeep();
            tracing::debug!("Pruner done did a prune");
        }
    });

    let metrics_storage = owned_storage.clone();
    spawn(async move {
        let storage = metrics_storage;
        loop {
            rt::time::sleep(Duration::from_secs(1)).await;
            gauge!("objects_stored_current").set(storage.len() as f64);
        }
    });

    HttpServer::new(move || {
        App::new()
            .app_data(owned_storage.clone())
            .app_data(metrics_handle.clone())
            .service(index)
            .service(info)
            .service(store)
            .service(take)
            .service(render_metrics)
    })
    .bind(bind)?
    .run()
    .await
}

/// `GET /` — serves the upload UI.
#[get("/")]
async fn index() -> impl Responder {
    counter!("requests_total", "route" => "/").increment(1);
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(INDEX_HTML)
}

/// `GET /info` — returns relevant server limits as JSON.
#[get("/info")]
async fn info() -> impl Responder {
    counter!("requests_total", "route" => "/info").increment(1);
    let config = Config::try_parse().unwrap_or_default();
    let body = serde_json::json!({
        "max_age_secs": config.max_age_secs,
        "max_object_size": config.max_object_size,
    });
    HttpResponse::Ok()
        .content_type("application/json")
        .body(body.to_string())
}

#[get("/metrics")]
async fn render_metrics(metrics: Data<PrometheusHandle>) -> impl Responder {
    counter!("requests_total", "route" => "/metrics").increment(1);
    HttpResponse::Ok().body(metrics.render())
}

/// `PUT /store` — encrypts the request body with a fresh AES-256-GCM key and stores it.
///
/// Returns `200 OK` with `{id}:{key_hex}` as a plain-text body on success, where `key_hex`
/// is the 64-character hex-encoded 256-bit key needed to retrieve and decrypt the object.
/// Returns `429 Too Many Requests` when the concurrency limit is reached,
/// `400 Bad Request` when the body exceeds `max_object_size`, `507 Insufficient Storage`
/// when storage is full,
/// and `500 Internal Server Error` on unexpected failures.
#[put("/store")]
async fn store(
    req: HttpRequest,
    bytes: Bytes,
    storage: web::Data<HashMap<Uuid, StoredObject>>,
) -> impl Responder {
    counter!("requests_total", "route" => "/store").increment(1);
    let config = Config::try_parse().unwrap_or_default();
    let Some(_permit) = LIMITER.try_acquire() else {
        counter!("responses_total", "route" => "/store", "status" => "429").increment(1);
        return HttpResponse::TooManyRequests().body("Slow down cowboy");
    };
    if bytes.len() > config.max_object_size as usize {
        counter!("responses_total", "route" => "/store", "status" => "400").increment(1);
        return HttpResponse::BadRequest().body("Too big");
    }
    if storage.len() >= config.max_objects as usize {
        counter!("responses_total", "route" => "/store", "status" => "507").increment(1);
        return HttpResponse::InsufficientStorage().body("Too many");
    }
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    tracing::info!(
        src = req.connection_info().realip_remote_addr(),
        size = bytes.len(),
        "Store Request"
    );

    let key = Aes256Gcm::generate_key(OsRng);
    let cipher = Aes256Gcm::new(&key);
    let nonce = Aes256Gcm::generate_nonce(OsRng);
    let Ok(ciphertext) = cipher.encrypt(&nonce, bytes.as_ref()) else {
        counter!("responses_total", "route" => "/store", "status" => "500").increment(1);
        return HttpResponse::InternalServerError().body("Oops");
    };

    let created = Instant::now();
    let object = StoredObject {
        created,
        content_type,
        nonce: nonce.into(),
        bytes: Bytes::from(ciphertext),
    };

    let mut id: Uuid = Uuid::nil();
    while id == Uuid::nil() {
        let new_id = Uuid::new_v4();
        if storage.contains_sync(&new_id) {
            continue;
        }
        id = new_id;
        break;
    }

    if storage.insert_async(id, object).await.is_err() {
        counter!("responses_total", "route" => "/store", "status" => "500").increment(1);
        return HttpResponse::InternalServerError().body("Oops");
    }

    counter!("bytes_stored").increment(bytes.len() as u64);
    counter!("objects_stored").increment(1);
    counter!("responses_total", "route" => "/store", "status" => "200").increment(1);
    HttpResponse::Ok().body(format!("{}/{}", id, hex::encode(key)))
}

/// `GET /take/{id}/{key}` — removes, decrypts, and returns the stored object.
///
/// `key` must be the 64-character hex-encoded AES-256 key returned by `PUT /store`.
/// On a cache hit with a valid key the decrypted bytes and original `Content-Type` are returned.
/// On a cache miss, an invalid id, an invalid key, or a decryption failure a random dummy
/// payload is returned with `Content-Type: text/plain`, making all failure modes
/// indistinguishable to observers.
/// Returns `429 Too Many Requests` when the concurrency limit is reached.
#[get("/take/{id}/{key}")]
async fn take(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    storage: web::Data<HashMap<Uuid, StoredObject>>,
) -> impl Responder {
    counter!("requests_total", "route" => "/take").increment(1);
    let Some(_permit) = LIMITER.try_acquire() else {
        counter!("responses_total", "route" => "/take", "status" => "429").increment(1);
        return HttpResponse::TooManyRequests().body("Slow down cowboy");
    };
    let (id_str, key_str) = path.into_inner();
    tracing::info!(
        src = req.connection_info().realip_remote_addr(),
        "Take Request"
    );

    let dummy = generate_dummy_object();

    let Ok(id) = Uuid::from_str(&id_str) else {
        counter!("responses_total", "route" => "/take", "status" => "200").increment(1);
        counter!("bytes_taken").increment(dummy.len() as u64);
        return HttpResponse::Ok().content_type("text/plain").body(dummy);
    };

    let key_bytes = match hex::decode(&key_str) {
        Ok(b) if b.len() == 32 => b,
        _ => {
            counter!("responses_total", "route" => "/take", "status" => "200").increment(1);
            counter!("bytes_taken").increment(dummy.len() as u64);
            return HttpResponse::Ok().content_type("text/plain").body(dummy);
        }
    };

    // Decrypt while holding a read guard before committing to removal, so a
    // wrong key does not consume the object.
    let result = storage
        .read_async(&id, |_, object| {
            let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
            let cipher = Aes256Gcm::new(&key);
            let nonce = Nonce::from(object.nonce);
            let plaintext = cipher.decrypt(&nonce, object.bytes.as_ref()).ok()?;
            Some((object.content_type.clone(), plaintext))
        })
        .await;

    let Some(Some((content_type, plaintext))) = result else {
        counter!("responses_total", "route" => "/take", "status" => "200").increment(1);
        counter!("bytes_taken").increment(dummy.len() as u64);
        return HttpResponse::Ok().content_type("text/plain").body(dummy);
    };

    storage.remove_async(&id).await;
    counter!("responses_total", "route" => "/take", "status" => "200").increment(1);
    counter!("bytes_taken").increment(plaintext.len() as u64);
    counter!("objects_taken").increment(1);
    HttpResponse::Ok()
        .content_type(content_type)
        .body(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};

    fn make_storage() -> Data<scc::HashMap<Uuid, StoredObject>> {
        Data::new(scc::HashMap::new())
    }

    /// Parse the `{uuid}/{key_hex}` token returned by PUT /store.
    fn parse_token(body: &[u8]) -> (String, String) {
        let s = std::str::from_utf8(body).expect("store response is not UTF-8");
        let (id, key) = s
            .split_once('/')
            .expect("store response missing '/' separator");
        (id.to_string(), key.to_string())
    }

    /// Returns true when `body` looks like a valid dummy payload.
    fn is_dummy(body: &[u8]) -> bool {
        body.len() >= MIN_DUMMY_OBJECT_SIZE
            && body.len() <= MAX_DUMMY_OBJECT_SIZE
            && body.iter().all(|&b| b >= b' ' && b <= b'~')
    }

    macro_rules! app {
        ($storage:expr) => {
            test::init_service(
                App::new()
                    .app_data($storage.clone())
                    .service(store)
                    .service(take),
            )
            .await
        };
    }

    // ── store ─────────────────────────────────────────────────────────────

    /// PUT /store returns 200 with a `{uuid}/{64-char-hex-key}` body.
    #[actix_web::test]
    async fn test_store_response_format() {
        let app = app!(make_storage());
        let req = test::TestRequest::put()
            .uri("/store")
            .set_payload("hello world")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body = test::read_body(resp).await;
        let (id, key) = parse_token(&body);
        assert!(Uuid::from_str(&id).is_ok(), "id is not a valid UUID: {id}");
        assert_eq!(key.len(), 64, "key should be 64 hex chars");
        assert!(
            key.chars().all(|c| c.is_ascii_hexdigit()),
            "key contains non-hex chars"
        );
    }

    // ── take: hit ─────────────────────────────────────────────────────────

    /// Valid id+key returns the original plaintext with the original Content-Type.
    #[actix_web::test]
    async fn test_take_returns_plaintext() {
        let storage = make_storage();
        let app = app!(storage);

        let req = test::TestRequest::put()
            .uri("/store")
            .insert_header(("content-type", "text/plain"))
            .set_payload("hello world")
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        let (id, key) = parse_token(&body);

        let req = test::TestRequest::get()
            .uri(&format!("/take/{id}/{key}"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        assert!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .starts_with("text/plain"),
            "expected text/plain content-type"
        );
        assert_eq!(&test::read_body(resp).await[..], b"hello world");
    }

    /// Binary payload survives the encrypt/decrypt round-trip byte-for-byte.
    #[actix_web::test]
    async fn test_binary_round_trip() {
        let storage = make_storage();
        let app = app!(storage);
        let payload: &[u8] = &[0x00, 0x01, 0x02, 0xFF, 0xFE];

        let req = test::TestRequest::put()
            .uri("/store")
            .insert_header(("content-type", "application/octet-stream"))
            .set_payload(payload.to_vec())
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        let (id, key) = parse_token(&body);

        let req = test::TestRequest::get()
            .uri(&format!("/take/{id}/{key}"))
            .to_request();
        assert_eq!(
            &test::read_body(test::call_service(&app, req).await).await[..],
            payload
        );
    }

    // ── take: miss / dummy ────────────────────────────────────────────────

    /// Taking an already-consumed object returns a valid dummy payload.
    #[actix_web::test]
    async fn test_take_consumed_returns_dummy() {
        let storage = make_storage();
        let app = app!(storage);

        let req = test::TestRequest::put()
            .uri("/store")
            .set_payload("once")
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        let (id, key) = parse_token(&body);

        // First take — consumes the object.
        let req = test::TestRequest::get()
            .uri(&format!("/take/{id}/{key}"))
            .to_request();
        test::call_service(&app, req).await;

        // Second take — object is gone, expect dummy.
        let req = test::TestRequest::get()
            .uri(&format!("/take/{id}/{key}"))
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        assert!(is_dummy(&body), "expected dummy, got {} bytes", body.len());
    }

    /// A non-UUID id returns a dummy payload.
    #[actix_web::test]
    async fn test_take_invalid_uuid_returns_dummy() {
        let app = app!(make_storage());
        let key = "a".repeat(64);
        let req = test::TestRequest::get()
            .uri(&format!("/take/not-a-uuid/{key}"))
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        assert!(is_dummy(&body), "expected dummy, got {} bytes", body.len());
    }

    /// A correctly-formatted but wrong key returns a dummy payload.
    #[actix_web::test]
    async fn test_take_wrong_key_returns_dummy() {
        let storage = make_storage();
        let app = app!(storage);

        let req = test::TestRequest::put()
            .uri("/store")
            .set_payload("secret")
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        let (id, _) = parse_token(&body);
        let wrong_key = "b".repeat(64);

        let req = test::TestRequest::get()
            .uri(&format!("/take/{id}/{wrong_key}"))
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        assert!(is_dummy(&body), "expected dummy, got {} bytes", body.len());
    }

    /// A wrong key attempt does NOT consume the object; the correct key still works after.
    #[actix_web::test]
    async fn test_wrong_key_does_not_consume_object() {
        let storage = make_storage();
        let app = app!(storage);

        let req = test::TestRequest::put()
            .uri("/store")
            .set_payload("secret")
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        let (id, key) = parse_token(&body);
        let wrong_key = "c".repeat(64);

        // Wrong key attempt.
        let req = test::TestRequest::get()
            .uri(&format!("/take/{id}/{wrong_key}"))
            .to_request();
        test::call_service(&app, req).await;

        // Correct key should still retrieve the object.
        let req = test::TestRequest::get()
            .uri(&format!("/take/{id}/{key}"))
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        assert_eq!(&body[..], b"secret");
    }

    /// A key that is valid hex but fewer than 32 bytes returns a dummy payload.
    #[actix_web::test]
    async fn test_take_short_key_returns_dummy() {
        let storage = make_storage();
        let app = app!(storage);

        let req = test::TestRequest::put()
            .uri("/store")
            .set_payload("data")
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        let (id, _) = parse_token(&body);

        let req = test::TestRequest::get()
            .uri(&format!("/take/{id}/deadbeef"))
            .to_request();
        let body = test::read_body(test::call_service(&app, req).await).await;
        assert!(is_dummy(&body), "expected dummy, got {} bytes", body.len());
    }
}
