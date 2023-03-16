use std::path::Path as StdPath;

use actix_files::NamedFile;
use actix_multipart::form::tempfile::TempFile;
use actix_multipart::form::MultipartForm;
use actix_web::rt::time::Instant;
use actix_web::{delete, get, post, put, web, Responder, Result};
use actix_web_validator::{Json, Path, Query};
use collection::operations::snapshot_ops::{SnapshotPriority, SnapshotRecover};
use reqwest::Url;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use storage::content_manager::errors::StorageError;
use storage::content_manager::snapshots::recover::do_recover_from_snapshot;
use storage::content_manager::snapshots::{
    do_create_full_snapshot, do_delete_collection_snapshot, do_delete_full_snapshot,
    do_list_full_snapshots, get_full_snapshot_path,
};
use storage::content_manager::toc::TableOfContent;
use storage::dispatcher::Dispatcher;
use uuid::Uuid;
use validator::Validate;

use super::CollectionPath;
use crate::actix::helpers::{
    collection_into_actix_error, process_response, storage_into_actix_error,
};
use crate::common::collections::*;

#[derive(Deserialize, Validate)]
struct SnapshotPath {
    #[serde(rename = "snapshot_name")]
    #[validate(length(min = 1))]
    name: String,
}

#[derive(Deserialize, Serialize, JsonSchema, Validate)]
pub struct SnapshotUploadingParam {
    pub wait: Option<bool>,
    pub priority: Option<SnapshotPriority>,
}

#[derive(Deserialize, Serialize, JsonSchema, Validate)]
pub struct SnapshottingParam {
    pub wait: Option<bool>,
}

#[derive(MultipartForm)]
pub struct SnapshottingForm {
    snapshot: TempFile,
}

// Actix specific code
pub async fn do_get_full_snapshot(
    dispatcher: &Dispatcher,
    snapshot_name: &str,
    wait: bool,
) -> Result<NamedFile> {
    let dispatcher = dispatcher.clone();
    let snapshot_name = snapshot_name.to_string();
    let task =
        tokio::spawn(async move { get_full_snapshot_path(dispatcher.toc(), &snapshot_name).await });

    if wait {
        let filename = task.await;
        if let Err(e) = filename {
            return Err(ErrorInternalServerError(e.to_string()));
        }
        let filename = filename.unwrap().map_err(storage_into_actix_error)?;
        Ok(NamedFile::open(filename)?)
    } else {
        Ok(NamedFile::open("not_found")?)
    }
}

pub fn do_save_uploaded_snapshot(
    toc: &TableOfContent,
    collection_name: &str,
    snapshot: TempFile,
) -> std::result::Result<Url, StorageError> {
    let filename = snapshot.file_name.unwrap_or(Uuid::new_v4().to_string());
    let path = StdPath::new(toc.snapshots_path())
        .join(collection_name)
        .join(filename);

    snapshot.file.persist(&path)?;

    let absolute_path = path.canonicalize()?;

    let snapshot_location = Url::from_file_path(&absolute_path).map_err(|_| {
        StorageError::service_error(format!(
            "Failed to convert path to URL: {}",
            absolute_path.display()
        ))
    })?;

    Ok(snapshot_location)
}

// Actix specific code
pub async fn do_get_snapshot(
    dispatcher: &Dispatcher,
    collection_name: &str,
    snapshot_name: &str,
    wait: bool,
) -> Result<NamedFile> {
    let dispatcher = dispatcher.clone();
    let collection_name = collection_name.to_string();
    let snapshot_name = snapshot_name.to_string();

    let task = tokio::spawn(async move {
        let collection = dispatcher
            .get_collection(&collection_name)
            .await
            .map_err(storage_into_actix_error)
            .unwrap();

        collection
            .get_snapshot_path(&snapshot_name)
            .await
            .map_err(collection_into_actix_error)
            .unwrap()
    });

    if wait {
        let result = task.await.unwrap();
        Ok(NamedFile::open(result)?)
    } else {
        Ok(NamedFile::open("not_found")?)
    }
}

#[get("/collections/{name}/snapshots")]
async fn list_snapshots(
    dispatcher: web::Data<Dispatcher>,
    path: web::Path<String>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let collection_name = path.into_inner();
    let timing = Instant::now();
    let wait = params.wait.unwrap_or(true);

    let response = do_list_snapshots(dispatcher.get_ref(), &collection_name, wait).await;
    process_response(response, timing)
}

#[post("/collections/{name}/snapshots")]
async fn create_snapshot(
    dispatcher: web::Data<Dispatcher>,
    path: web::Path<String>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let collection_name = path.into_inner();
    let wait = params.wait.unwrap_or(true);

    let timing = Instant::now();
    let response = do_create_snapshot(dispatcher.get_ref(), &collection_name, wait).await;
    process_response(response, timing)
}

#[post("/collections/{name}/snapshots/upload")]
async fn upload_snapshot(
    dispatcher: web::Data<Dispatcher>,
    collection: Path<CollectionPath>,
    MultipartForm(form): MultipartForm<SnapshottingForm>,
    params: Query<SnapshotUploadingParam>,
) -> impl Responder {
    let timing = Instant::now();
    let snapshot = form.snapshot;
    let wait = params.wait.unwrap_or(true);

    let snapshot_location =
        match do_save_uploaded_snapshot(dispatcher.get_ref(), &collection.name, snapshot) {
            Ok(location) => location,
            Err(err) => return process_response(Err(err), timing),
        };

    let snapshot_recover = SnapshotRecover {
        location: snapshot_location,
        priority: params.priority,
    };

    let response = do_recover_from_snapshot(
        dispatcher.get_ref(),
        &collection.name,
        snapshot_recover,
        wait,
    )
    .await;
    process_response(response, timing)
}

#[put("/collections/{name}/snapshots/recover")]
async fn recover_from_snapshot(
    dispatcher: web::Data<Dispatcher>,
    collection: Path<CollectionPath>,
    request: Json<SnapshotRecover>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let timing = Instant::now();
    let snapshot_recover = request.into_inner();
    let wait = params.wait.unwrap_or(true);

    let response = do_recover_from_snapshot(
        dispatcher.get_ref(),
        &collection.name,
        snapshot_recover,
        wait,
    )
    .await;
    process_response(response, timing)
}

#[get("/collections/{name}/snapshots/{snapshot_name}")]
async fn get_snapshot(
    dispatcher: web::Data<Dispatcher>,
    path: web::Path<(String, String)>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let wait = params.wait.unwrap_or(true);
    let (collection_name, snapshot_name) = path.into_inner();
    do_get_snapshot(dispatcher.get_ref(), &collection_name, &snapshot_name, wait).await
}

#[get("/snapshots")]
async fn list_full_snapshots(
    dispatcher: web::Data<Dispatcher>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let timing = Instant::now();
    let wait = params.wait.unwrap_or(true);
    let response = do_list_full_snapshots(dispatcher.get_ref(), wait).await;
    process_response(response, timing)
}

#[post("/snapshots")]
async fn create_full_snapshot(
    dispatcher: web::Data<Dispatcher>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let timing = Instant::now();
    let wait = params.wait.unwrap_or(true);
    let response = do_create_full_snapshot(dispatcher.get_ref(), wait).await;
    process_response(response, timing)
}

#[get("/snapshots/{snapshot_name}")]
async fn get_full_snapshot(
    dispatcher: web::Data<Dispatcher>,
    path: web::Path<String>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let snapshot_name = path.into_inner();
    let wait = params.wait.unwrap_or(true);
    do_get_full_snapshot(dispatcher.get_ref(), &snapshot_name, wait).await
}

#[delete("/snapshots/{snapshot_name}")]
async fn delete_full_snapshot(
    dispatcher: web::Data<Dispatcher>,
    path: web::Path<String>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let snapshot_name = path.into_inner();
    let timing = Instant::now();
    let wait = params.wait.unwrap_or(true);
    let response = do_delete_full_snapshot(dispatcher.get_ref(), &snapshot_name, wait).await;
    process_response(response, timing)
}

#[delete("/collections/{name}/snapshots/{snapshot_name}")]
async fn delete_collection_snapshot(
    dispatcher: web::Data<Dispatcher>,
    path: web::Path<(String, String)>,
    params: Query<SnapshottingParam>,
) -> impl Responder {
    let (collection_name, snapshot_name) = path.into_inner();
    let timing = Instant::now();
    let wait = params.wait.unwrap_or(true);
    let response =
        do_delete_collection_snapshot(dispatcher.get_ref(), &collection_name, &snapshot_name, wait)
            .await;
    process_response(response, timing)
}

// Configure services
pub fn config_snapshots_api(cfg: &mut web::ServiceConfig) {
    cfg.service(list_snapshots)
        .service(create_snapshot)
        .service(upload_snapshot)
        .service(recover_from_snapshot)
        .service(get_snapshot)
        .service(list_full_snapshots)
        .service(create_full_snapshot)
        .service(get_full_snapshot)
        .service(delete_full_snapshot)
        .service(delete_collection_snapshot);
}
