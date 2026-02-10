use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Multipart, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum::extract::DefaultBodyLimit;
use async_stream::try_stream;
use futures_core::Stream;
use clap::Parser;
use get_if_addrs::get_if_addrs;
use image::{ImageFormat, ImageReader};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::Arc,
};
use tokio::{fs, io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener, sync::RwLock};
use tokio_util::io::ReaderStream;
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};
use walkdir::WalkDir;

const ADMIN_HTML: &str = include_str!("../assets/admin.html");
const USER_HTML: &str = include_str!("../assets/user.html");

#[derive(Parser, Debug)]
#[command(name = "dropship", version, about = "Dropship: Ad-hoc file transfer")]
struct Args {
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,

    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,
}

#[derive(Clone)]
struct AppState {
    base_dir: Arc<RwLock<Option<PathBuf>>>,
    token: Arc<RwLock<Option<String>>>,
    packs: Arc<RwLock<HashMap<String, Vec<String>>>>,
    port: u16,
}

#[derive(Serialize)]
struct StatusResponse {
    base_dir: Option<String>,
    token_set: bool,
    ips: Vec<String>,
    port: u16,
}

#[derive(Serialize)]
struct TokenResponse {
    token: Option<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Deserialize)]
struct SetDirRequest {
    path: String,
}

#[derive(Deserialize)]
struct SetTokenRequest {
    token: Option<String>,
}

#[derive(Deserialize)]
struct ListQuery {
    path: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize)]
struct DownloadQuery {
    path: Option<String>,
    pack: Option<String>,
    archive: Option<String>,
    token: Option<String>,
}

#[derive(Deserialize)]
struct PackRequest {
    paths: Vec<String>,
    archive: Option<String>,
    token: Option<String>,
}

#[derive(Serialize)]
struct PackResponse {
    id: String,
    archive: Option<String>,
}

#[derive(Deserialize)]
struct UploadQuery {
    path: Option<String>,
    token: Option<String>,
}

#[derive(Serialize)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
}

struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(ErrorResponse {
            error: self.message,
        });
        (self.status, body).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();

    let mut port = 6767u16;
    let listener = loop {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => break listener,
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                port = port.saturating_add(1);
            }
            Err(err) => return Err(err.into()),
        }
    };

    let base_dir = if let Some(dir) = args.dir {
        Some(dir)
    } else {
        None
    };

    let state = Arc::new(AppState {
        base_dir: Arc::new(RwLock::new(base_dir)),
        token: Arc::new(RwLock::new(args.token)),
        packs: Arc::new(RwLock::new(HashMap::new())),
        port,
    });

    info!("Dropship listening on http://127.0.0.1:{port}");

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/admin/dir", post(admin_set_dir))
        .route("/api/admin/token", get(admin_get_token).post(admin_set_token))
        .route("/api/admin/list", get(admin_list_dirs))
        .route("/api/list", get(list_files))
        .route("/api/download", get(download))
        .route("/api/pack", post(create_pack))
        .route("/api/file", get(view_file))
        .route("/api/thumbnail", get(thumbnail))
        .route("/api/upload", post(upload))
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
        .into_make_service_with_connect_info::<SocketAddr>();

    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(
    State(_state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    if is_admin(addr) {
        Html(ADMIN_HTML)
    } else {
        Html(USER_HTML)
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    Json(build_status(&state).await)
}

async fn admin_get_token(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<TokenResponse>, AppError> {
    ensure_admin(addr)?;
    let token = state.token.read().await.clone();
    Ok(Json(TokenResponse { token }))
}

async fn admin_set_token(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<SetTokenRequest>,
) -> Result<Json<TokenResponse>, AppError> {
    ensure_admin(addr)?;
    let token = payload.token.filter(|t| !t.trim().is_empty()).or_else(|| {
        Some(
            rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(24)
                .map(char::from)
                .collect(),
        )
    });
    *state.token.write().await = token.clone();
    Ok(Json(TokenResponse { token }))
}

async fn admin_set_dir(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<SetDirRequest>,
) -> Result<Json<StatusResponse>, AppError> {
    ensure_admin(addr)?;
    let path = PathBuf::from(payload.path);
    if !path.is_dir() {
        return Err(AppError::bad_request("Directory does not exist"));
    }
    *state.base_dir.write().await = Some(path);
    Ok(Json(build_status(&state).await))
}

#[derive(Deserialize)]
struct AdminListQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct DirEntry {
    name: String,
    path: String,
}

async fn admin_list_dirs(
    State(_state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<AdminListQuery>,
) -> Result<Json<Vec<DirEntry>>, AppError> {
    ensure_admin(addr)?;
    let base = query
        .path
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
    let base = if base.is_dir() { base } else { PathBuf::from("/") };

    let mut entries = Vec::new();
    let mut dir = fs::read_dir(&base)
        .await
        .map_err(|_| AppError::bad_request("Unable to read directory"))?;
    while let Some(entry) = dir
        .next_entry()
        .await
        .map_err(|_| AppError::internal("Failed to read directory"))?
    {
        let metadata = entry
            .metadata()
            .await
            .map_err(|_| AppError::internal("Failed to read metadata"))?;
        if metadata.is_dir() {
            let path = entry.path();
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: path.to_string_lossy().to_string(),
            });
        }
    }
    Ok(Json(entries))
}

async fn build_status(state: &AppState) -> StatusResponse {
    let base_dir = state
        .base_dir
        .read()
        .await
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());

    let token_set = state.token.read().await.is_some();

    let ips = get_if_addrs()
        .map(|ifs| {
            ifs.into_iter()
                .filter(|iface| !iface.ip().is_loopback())
                .map(|iface| iface.ip().to_string())
                .collect()
        })
        .unwrap_or_default();

    StatusResponse {
        base_dir,
        token_set,
        ips,
        port: state.port,
    }
}

async fn list_files(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<FileEntry>>, AppError> {
    let is_admin = is_admin(addr);
    ensure_access(&state, is_admin, &headers, query.token.as_deref()).await?;

    let base = get_base_dir(&state).await?;
    let rel = query.path.unwrap_or_default();
    let target = resolve_path(&base, &rel)?;

    let mut entries = Vec::new();
    let mut dir = fs::read_dir(&target)
        .await
        .map_err(|_| AppError::bad_request("Unable to read directory"))?;
    while let Some(entry) = dir
        .next_entry()
        .await
        .map_err(|_| AppError::internal("Failed to read directory"))?
    {
        let metadata = entry
            .metadata()
            .await
            .map_err(|_| AppError::internal("Failed to read metadata"))?;
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            is_dir: metadata.is_dir(),
            size: if metadata.is_file() { metadata.len() } else { 0 },
        });
    }
    Ok(Json(entries))
}

async fn upload(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<Json<StatusResponse>, AppError> {
    let is_admin = is_admin(addr);
    ensure_access(&state, is_admin, &headers, query.token.as_deref()).await?;

    let base = get_base_dir(&state).await?;
    let rel = query.path.unwrap_or_default();
    let target_dir = resolve_path(&base, &rel)?;
    fs::create_dir_all(&target_dir)
        .await
        .map_err(|_| AppError::internal("Failed to create upload directory"))?;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::bad_request("Invalid multipart"))?
    {
        let Some(file_name) = field.file_name().map(|s| s.to_string()) else {
            continue;
        };
        let file_path = target_dir.join(file_name);
        let mut file = fs::File::create(file_path)
            .await
            .map_err(|_| AppError::internal("Failed to create file"))?;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|_| AppError::bad_request("Failed to read upload"))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|_| AppError::internal("Failed to write file"))?;
        }
    }

    Ok(status(State(state)).await)
}

async fn download(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, AppError> {
    let is_admin = is_admin(addr);
    ensure_access(&state, is_admin, &headers, query.token.as_deref()).await?;

    let base = get_base_dir(&state).await?;
    let (paths, archive_override) = if let Some(pack_id) = query.pack.as_deref() {
        let mut packs = state.packs.write().await;
        let Some(paths) = packs.remove(pack_id) else {
            return Err(AppError::bad_request("Invalid pack"));
        };
        (paths, query.archive.clone())
    } else if let Some(path) = query.path.clone() {
        (vec![path], None)
    } else {
        return Err(AppError::bad_request("No paths provided"));
    };

    let mut resolved = Vec::new();
    for path in &paths {
        resolved.push(resolve_path(&base, path)?);
    }

    let mut need_archive = query.archive.is_some() || archive_override.is_some();
    if resolved.len() == 1 {
        let meta = fs::metadata(&resolved[0])
            .await
            .map_err(|_| AppError::bad_request("File not found"))?;
        if meta.is_dir() {
            need_archive = true;
        } else if !need_archive {
            let name = resolved[0]
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download");
            return stream_file(&resolved[0], Some(name)).await;
        }
    } else {
        need_archive = true;
    }

    if !need_archive {
        return Err(AppError::bad_request("Invalid download request"));
    }

    let archive_type = archive_override
        .or(query.archive)
        .unwrap_or_else(|| "zip".to_string());
    let temp_path = if archive_type == "tar" {
        build_tar(&base, &resolved).await?
    } else {
        build_zip(&base, &resolved).await?
    };

    let file_name = if archive_type == "tar" {
        "dropship.tar"
    } else {
        "dropship.zip"
    };
    stream_temp_file(&temp_path, Some(file_name)).await
}

async fn create_pack(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<PackRequest>,
) -> Result<Json<PackResponse>, AppError> {
    let is_admin = is_admin(addr);
    ensure_access(&state, is_admin, &headers, payload.token.as_deref()).await?;
    if payload.paths.is_empty() {
        return Err(AppError::bad_request("No paths provided"));
    }

    let id: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(char::from)
        .collect();
    state.packs.write().await.insert(id.clone(), payload.paths);
    Ok(Json(PackResponse {
        id,
        archive: payload.archive,
    }))
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
    token: Option<String>,
}

async fn view_file(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> Result<Response, AppError> {
    let is_admin = is_admin(addr);
    ensure_access(&state, is_admin, &headers, query.token.as_deref()).await?;

    let base = get_base_dir(&state).await?;
    let target = resolve_path(&base, &query.path)?;
    stream_file(&target, None).await
}

#[derive(Deserialize)]
struct ThumbnailQuery {
    path: String,
    token: Option<String>,
}

async fn thumbnail(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<ThumbnailQuery>,
) -> Result<Response, AppError> {
    let is_admin = is_admin(addr);
    ensure_access(&state, is_admin, &headers, query.token.as_deref()).await?;

    let base = get_base_dir(&state).await?;
    let target = resolve_path(&base, &query.path)?;
    let target_clone = target.clone();

    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, AppError> {
        let img = ImageReader::open(&target_clone)
            .map_err(|_| AppError::bad_request("Invalid image"))?
            .decode()
            .map_err(|_| AppError::bad_request("Invalid image"))?;
        let thumb = img.thumbnail(128, 128);
        let mut buf = Vec::new();
        thumb
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Jpeg)
            .map_err(|_| AppError::internal("Failed to encode image"))?;
        Ok(buf)
    })
    .await
    .map_err(|_| AppError::internal("Thumbnail task failed"))??;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
    Ok((headers, bytes).into_response())
}

async fn stream_file(path: &Path, download_name: Option<&str>) -> Result<Response, AppError> {
    let file = fs::File::open(path)
        .await
        .map_err(|_| AppError::bad_request("File not found"))?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime.as_ref().parse().unwrap());
    if let Some(name) = download_name {
        headers.insert(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\"").parse().unwrap(),
        );
    }
    Ok((headers, body).into_response())
}

async fn stream_temp_file(path: &Path, download_name: Option<&str>) -> Result<Response, AppError> {
    let file = fs::File::open(path)
        .await
        .map_err(|_| AppError::bad_request("File not found"))?;
    let path = path.to_path_buf();
    let mime_path = path.clone();
    let stream: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = Box::pin(try_stream! {
        let mut file = file;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            yield Bytes::copy_from_slice(&buf[..n]);
        }
        let _ = fs::remove_file(&path).await;
    });
    let body = Body::from_stream(stream);
    let mime = mime_guess::from_path(&mime_path).first_or_octet_stream();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime.as_ref().parse().unwrap());
    if let Some(name) = download_name {
        headers.insert(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\"").parse().unwrap(),
        );
    }
    Ok((headers, body).into_response())
}

fn is_admin(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}

fn ensure_admin(addr: SocketAddr) -> Result<(), AppError> {
    if is_admin(addr) {
        Ok(())
    } else {
        Err(AppError::forbidden("Admin only"))
    }
}

async fn ensure_access(
    state: &AppState,
    is_admin: bool,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<(), AppError> {
    if is_admin {
        return Ok(());
    }
    let token_guard = state.token.read().await;
    let Some(expected) = token_guard.as_ref() else {
        return Ok(());
    };
    let header_token = headers
        .get("x-dropship-token")
        .and_then(|v| v.to_str().ok());
    let provided = header_token.or(query_token);
    if provided == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(AppError::forbidden("Invalid or missing token"))
    }
}

async fn get_base_dir(state: &AppState) -> Result<PathBuf, AppError> {
    state
        .base_dir
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::bad_request("Serving directory not set"))
}

fn resolve_path(base: &Path, rel: &str) -> Result<PathBuf, AppError> {
    if rel.is_empty() {
        return Ok(base.to_path_buf());
    }

    let candidate = Path::new(rel);
    for component in candidate.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(AppError::bad_request("Invalid path"));
            }
            _ => {}
        }
    }
    Ok(base.join(candidate))
}

async fn build_zip(base: &Path, paths: &[PathBuf]) -> Result<PathBuf, AppError> {
    let temp_path = temp_archive_path("zip");
    let temp_path_clone = temp_path.clone();
    let base = base.to_path_buf();
    let paths = paths.to_vec();

    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let file = std::fs::File::create(&temp_path_clone)
            .map_err(|_| AppError::internal("Failed to create zip"))?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Deflated);

        for path in paths {
            if path.is_dir() {
                for entry in WalkDir::new(&path) {
                    let entry = entry.map_err(|_| AppError::internal("Failed to read dir"))?;
                    let entry_path = entry.path();
                    if entry_path.is_dir() {
                        continue;
                    }
                    let name = entry_path.strip_prefix(&base).map_err(|_| AppError::internal("Invalid path"))?;
                    zip.start_file(name.to_string_lossy(), options)
                        .map_err(|_| AppError::internal("Failed to write zip"))?;
                    let mut f = std::fs::File::open(entry_path)
                        .map_err(|_| AppError::internal("Failed to open file"))?;
                    std::io::copy(&mut f, &mut zip)
                        .map_err(|_| AppError::internal("Failed to write zip"))?;
                }
            } else {
                let name = path
                    .strip_prefix(&base)
                    .map_err(|_| AppError::internal("Invalid path"))?;
                zip.start_file(name.to_string_lossy(), options)
                    .map_err(|_| AppError::internal("Failed to write zip"))?;
                let mut f = std::fs::File::open(&path)
                    .map_err(|_| AppError::internal("Failed to open file"))?;
                std::io::copy(&mut f, &mut zip)
                    .map_err(|_| AppError::internal("Failed to write zip"))?;
            }
        }
        zip.finish()
            .map_err(|_| AppError::internal("Failed to finish zip"))?;
        Ok(())
    })
    .await
    .map_err(|_| AppError::internal("Zip task failed"))??;

    Ok(temp_path)
}

async fn build_tar(base: &Path, paths: &[PathBuf]) -> Result<PathBuf, AppError> {
    let temp_path = temp_archive_path("tar");
    let temp_path_clone = temp_path.clone();
    let base = base.to_path_buf();
    let paths = paths.to_vec();

    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let file = std::fs::File::create(&temp_path_clone)
            .map_err(|_| AppError::internal("Failed to create tar"))?;
        let mut builder = tar::Builder::new(file);
        for path in paths {
            if path.is_dir() {
                for entry in WalkDir::new(&path) {
                    let entry = entry.map_err(|_| AppError::internal("Failed to read dir"))?;
                    let entry_path = entry.path();
                    if entry_path.is_dir() {
                        continue;
                    }
                    let name = entry_path.strip_prefix(&base).map_err(|_| AppError::internal("Invalid path"))?;
                    builder
                        .append_path_with_name(entry_path, name)
                        .map_err(|_| AppError::internal("Failed to write tar"))?;
                }
            } else {
                let name = path
                    .strip_prefix(&base)
                    .map_err(|_| AppError::internal("Invalid path"))?;
                builder
                    .append_path_with_name(&path, name)
                    .map_err(|_| AppError::internal("Failed to write tar"))?;
            }
        }
        builder.finish().map_err(|_| AppError::internal("Failed to finish tar"))?;
        Ok(())
    })
    .await
    .map_err(|_| AppError::internal("Tar task failed"))??;

    Ok(temp_path)
}

fn temp_archive_path(ext: &str) -> PathBuf {
    let rand: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .collect();
    std::env::temp_dir().join(format!("dropship-{rand}.{ext}"))
}
