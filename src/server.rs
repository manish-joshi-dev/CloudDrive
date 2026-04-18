use axum::{Router, response::{IntoResponse, Json}, routing::{get, post, delete},extract::{State,Path,Multipart},http::StatusCode,};
use serde_json::{json, Value};
use std::net::SocketAddr;
use dotenv::dotenv;
use tracing::{debug, info};
use aws_sdk_s3::Client;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use aws_sdk_s3::primitives::ByteStream;
struct AppState {
    s3: Client,
    bucket: String,
}



#[derive(Serialize,Debug)]
struct FileEntry {
    name: String,
    size: String,
    kind: String,
    modified: String,
}

fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn get_kind(name: &str) -> String {
    name.rsplit('.')
        .next()
        .unwrap_or("FILE")
        .to_uppercase()
}

pub async fn run() {
    dotenv().ok();
   let config = aws_config::from_env()
    .load()
    .await;
    // debug!(config = ?config, "AWS config loaded");
    debug!("{:?}",config);
    
    // println!("{:?}",config);
    let s3 = Client::new(&config);
    let bucket = std::env::var("S3_BUCKET").expect("S3_BUCKET not set in .env");
    let state = Arc::new(AppState { s3, bucket });


    let app = Router::new()
        .route("/files",           get(list_files))
        .route("/upload",          post(upload_file))
        .route("/download/*name",  get(download_file))
        .route("/delete/*name",    delete(delete_file))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn list_files(State(state):State<Arc<AppState>>)->impl IntoResponse{

    println!("GET /files called");
    let result = state.s3
        .list_objects_v2()
        .bucket(&state.bucket)
        .send()
        .await;


    match result {
        Ok(output) => {
            let files: Vec<FileEntry> = output
                .contents()
                .iter()
                .map(|obj| {
                    let name = obj.key().unwrap_or("unknown").to_string();
                    let size = format_size(obj.size().unwrap_or(0));
                    let kind = get_kind(&name);
                    let modified = obj
                        .last_modified()
                        .map(|t| t.to_string())
                        .unwrap_or_default();
                    FileEntry { name, size, kind, modified }
                })
                .collect();
            println!("hi {:?}",files);

            Json(json!(files)).into_response()
        }
        Err(e) => {
            let detail = match e.as_service_error() {
        Some(svc_err) => format!("{:?}", svc_err),
        None => format!("{:?}", e),  // network/config errors
    };
    println!("Error listing files: {:?}", e);   // full debug print
    println!("Service detail: {}", detail);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    println!("POST /upload called");
    let mut uploaded = Vec::new();

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = match field.file_name() {
            Some(n) => n.to_string(),
            None => continue,
        };

        let data = match field.bytes().await {
            Ok(d) => d,
            Err(e) => {
                println!("Error reading field: {}", e);
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": "failed to read file" }))).into_response();
            }
        };

        let result = state.s3
            .put_object()
            .bucket(&state.bucket)
            .key(&name)
            .body(ByteStream::from(data))
            .send()
            .await;

        match result {
            Ok(_) => {
                println!("Uploaded: {}", name);
                uploaded.push(name);
            }
            Err(e) => {
                println!("Upload error: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
            }
        }
    }

    if uploaded.is_empty() {
        (StatusCode::BAD_REQUEST, Json(json!({ "error": "no file found in request" }))).into_response()
    } else {
        Json(json!({ "message": "uploaded", "count": uploaded.len(), "names": uploaded })).into_response()
    }
}

async fn download_file(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    println!("GET /download/{} called", name);

    let result = state.s3
        .get_object()
        .bucket(&state.bucket)
        .key(&name)
        .send()
        .await;

    match result {
        Ok(output) => {
            let bytes = output.body.collect().await.unwrap().into_bytes();
            bytes.into_response()
        }
        Err(e) => {
            println!("Download error: {}", e);
            (StatusCode::NOT_FOUND, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

async fn delete_file(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    println!("DELETE /delete/{} called", name);

    let result = state.s3
        .delete_object()
        .bucket(&state.bucket)
        .key(&name)
        .send()
        .await;

    match result {
        Ok(_) => Json(json!({ "message": "deleted", "name": name })).into_response(),
        Err(e) => {
            println!("Delete error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}




