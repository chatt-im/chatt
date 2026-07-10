use std::fs;
use std::io;
use std::path::PathBuf;

use crate::config::ServerConfig;
use crate::http::date;
use crate::http::mime;
use crate::http::request::{ByteRange, Request};
use crate::http::response::{self, ContentRange, FileResponse, PreparedResponse};
use crate::router::MountKind;

pub(crate) struct FileTask {
    root: PathBuf,
    kind: MountKind,
    relative_path: String,
    request: Request,
    config: ServerConfig,
}

impl FileTask {
    pub(crate) fn new(
        root: PathBuf,
        kind: MountKind,
        relative_path: String,
        request: Request,
        config: ServerConfig,
    ) -> Self {
        Self {
            root,
            kind,
            relative_path,
            request,
            config,
        }
    }

    pub(crate) fn serve(self) -> PreparedResponse {
        match resolve_path(&self) {
            ResolveResult::Path(path) => serve_path(path, &self.request, &self.config),
            ResolveResult::NotFound => response::error(
                404,
                "Not Found",
                "The URL you requested was not found.",
                self.request.keep_alive,
                self.config.http_timeout,
                self.request.is_head(),
            ),
            ResolveResult::Forbidden => response::error(
                403,
                "Forbidden",
                "You do not have permission to access this file.",
                self.request.keep_alive,
                self.config.http_timeout,
                self.request.is_head(),
            ),
        }
    }
}

fn resolve_path(task: &FileTask) -> ResolveResult {
    let Ok(root) = fs::canonicalize(&task.root) else {
        return ResolveResult::NotFound;
    };
    let mut candidate = task.root.clone();
    for component in task.relative_path.split('/') {
        if component.is_empty() {
            continue;
        }
        candidate.push(component);
    }

    if task.kind == MountKind::StaticDir {
        if task.relative_path.is_empty() || task.relative_path.ends_with('/') {
            candidate.push("index.html");
        } else if candidate.is_dir() {
            candidate.push("index.html");
        }
    } else if task.relative_path.is_empty()
        || task.relative_path.ends_with('/')
        || candidate.is_dir()
    {
        return ResolveResult::NotFound;
    }

    let Ok(canonical) = fs::canonicalize(&candidate) else {
        return ResolveResult::NotFound;
    };
    if !canonical.starts_with(&root) {
        return ResolveResult::Forbidden;
    }
    ResolveResult::Path(canonical)
}

enum ResolveResult {
    Path(PathBuf),
    NotFound,
    Forbidden,
}

fn serve_path(path: PathBuf, request: &Request, config: &ServerConfig) -> PreparedResponse {
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) => return file_error(error, request, config),
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return not_found(request, config),
        Err(error) => return file_error(error, request, config),
    };

    let last_modified = metadata
        .modified()
        .ok()
        .and_then(date::system_time_http_date);
    if let (Some(if_modified_since), Some(last_modified)) =
        (request.if_modified_since(), last_modified)
        && if_modified_since == last_modified.as_bytes()
    {
        return response::not_modified(
            request.keep_alive,
            config.http_timeout,
            Some(last_modified),
        );
    }

    let size = metadata.len();
    let content_type = mime::content_type(&path);

    if let Some(range) = request.range() {
        match apply_range(range, size) {
            Some((from, to)) => {
                let len = to - from + 1;
                response::file(FileResponse {
                    status: 206,
                    reason: "Partial Content",
                    file: Some(file),
                    offset: from,
                    len,
                    content_type,
                    last_modified,
                    content_range: Some(ContentRange::Satisfied { from, to, size }),
                    keep_alive: request.keep_alive,
                    timeout: config.http_timeout,
                    header_only: request.is_head(),
                })
            }
            None => response::file(FileResponse {
                status: 416,
                reason: "Range Not Satisfiable",
                file: None,
                offset: 0,
                len: 0,
                content_type,
                last_modified,
                content_range: Some(ContentRange::Unsatisfied { size }),
                keep_alive: request.keep_alive,
                timeout: config.http_timeout,
                header_only: true,
            }),
        }
    } else {
        response::file(FileResponse {
            status: 200,
            reason: "OK",
            file: Some(file),
            offset: 0,
            len: size,
            content_type,
            last_modified,
            content_range: None,
            keep_alive: request.keep_alive,
            timeout: config.http_timeout,
            header_only: request.is_head(),
        })
    }
}

pub(crate) fn apply_range(range: ByteRange, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let (from, mut to) = match (range.start, range.end) {
        (Some(from), Some(to)) => (from, to),
        (Some(from), None) => (from, size - 1),
        (None, Some(suffix_len)) => {
            let from = size.saturating_sub(suffix_len);
            (from, size - 1)
        }
        (None, None) => return None,
    };
    if to >= size {
        to = size - 1;
    }
    if from >= size || to < from {
        return None;
    }
    Some((from, to))
}

fn file_error(error: io::Error, request: &Request, config: &ServerConfig) -> PreparedResponse {
    match error.kind() {
        io::ErrorKind::NotFound => response::error(
            404,
            "Not Found",
            "The URL you requested was not found.",
            request.keep_alive,
            config.http_timeout,
            request.is_head(),
        ),
        io::ErrorKind::PermissionDenied => response::error(
            403,
            "Forbidden",
            "You do not have permission to access this file.",
            request.keep_alive,
            config.http_timeout,
            request.is_head(),
        ),
        _ => response::error(
            500,
            "Internal Server Error",
            "The URL you requested cannot be returned.",
            false,
            config.http_timeout,
            request.is_head(),
        ),
    }
}

fn not_found(request: &Request, config: &ServerConfig) -> PreparedResponse {
    response::error(
        404,
        "Not Found",
        "The URL you requested was not found.",
        request.keep_alive,
        config.http_timeout,
        request.is_head(),
    )
}
