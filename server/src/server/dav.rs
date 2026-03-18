use std::{io::SeekFrom, sync::Arc, time::SystemTime};

use bytes::Bytes;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::stream;
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::{
    kdbx::{build_kdbx_sync, parse_kdbx_sync},
    store::MAIN_BRANCH,
};

use super::state::AppState;

pub(super) const DB_FILE: &str = "database.kdbx";

#[derive(Debug, Clone)]
struct FileMeta {
    len: u64,
    modified: SystemTime,
}

#[derive(Debug, Clone)]
struct DirMeta;

impl DavMetaData for FileMeta {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }

    fn is_dir(&self) -> bool {
        false
    }
}

impl DavMetaData for DirMeta {
    fn len(&self) -> u64 {
        0
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(SystemTime::now())
    }

    fn is_dir(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
struct DbFileEntry;

impl DavDirEntry for DbFileEntry {
    fn name(&self) -> Vec<u8> {
        b"database.kdbx".to_vec()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(futures_util::future::ready(Ok(Box::new(FileMeta {
            len: 0,
            modified: SystemTime::now(),
        })
            as Box<dyn DavMetaData>)))
    }
}

#[derive(Debug)]
struct ReadFile {
    data: Bytes,
    pos: usize,
}

impl DavFile for ReadFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let len = self.data.len() as u64;
        Box::pin(futures_util::future::ready(Ok(Box::new(FileMeta {
            len,
            modified: SystemTime::now(),
        })
            as Box<dyn DavMetaData>)))
    }

    fn write_buf(&mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(futures_util::future::ready(Err(FsError::Forbidden)))
    }

    fn write_bytes(&mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(futures_util::future::ready(Err(FsError::Forbidden)))
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        let start = self.pos;
        let end = (self.pos + count).min(self.data.len());
        let slice = self.data.slice(start..end);
        self.pos = end;
        Box::pin(futures_util::future::ready(Ok(slice)))
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        let len = self.data.len() as u64;
        let new_pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => (len as i64 + n).max(0) as u64,
            SeekFrom::Current(n) => (self.pos as i64 + n).max(0) as u64,
        };
        self.pos = (new_pos as usize).min(self.data.len());
        Box::pin(futures_util::future::ready(Ok(new_pos)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(futures_util::future::ready(Ok(())))
    }
}

#[derive(Debug)]
struct WriteFile {
    buf: Vec<u8>,
    state: AppState,
    client_id: String,
}

impl DavFile for WriteFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let len = self.buf.len() as u64;
        Box::pin(futures_util::future::ready(Ok(Box::new(FileMeta {
            len,
            modified: SystemTime::now(),
        })
            as Box<dyn DavMetaData>)))
    }

    fn write_buf(&mut self, mut buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        use bytes::Buf;

        while buf.has_remaining() {
            let chunk = buf.chunk();
            let len = chunk.len();
            self.buf.extend_from_slice(chunk);
            buf.advance(len);
        }
        Box::pin(futures_util::future::ready(Ok(())))
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        self.buf.extend_from_slice(&buf);
        Box::pin(futures_util::future::ready(Ok(())))
    }

    fn read_bytes(&mut self, _count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(futures_util::future::ready(Err(FsError::Forbidden)))
    }

    fn seek(&mut self, _pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(futures_util::future::ready(Err(FsError::NotImplemented)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        let bytes = self.buf.clone();
        let state = self.state.clone();
        let client_id = self.client_id.clone();

        Box::pin(async move {
            let config = Arc::clone(&state.config);

            let storage = spawn_blocking(move || parse_kdbx_sync(&bytes, &config.database))
                .await
                .map_err(|_| FsError::GeneralFailure)?
                .map_err(|e| {
                    warn!("Client '{}': failed to parse KDBX: {e:#}", client_id);
                    FsError::Forbidden
                })?;

            state
                .store
                .lock()
                .await
                .process_client_write(client_id.clone(), storage)
                .await
                .map(|updated_branches| {
                    let main_advanced = updated_branches.iter().any(|branch| branch == MAIN_BRANCH);
                    state.notify_branches(updated_branches.iter());
                    if main_advanced {
                        let push_state = state.clone();
                        tokio::spawn(async move {
                            if let Err(err) = push_state.dispatch_push_notifications().await {
                                warn!("push delivery task failed: {err:#}");
                            }
                        });
                    }
                })
                .map_err(|e| {
                    warn!("Client '{}': git write failed: {e:#}", client_id);
                    FsError::GeneralFailure
                })?;

            info!("Client '{}' write committed", client_id);
            Ok(())
        })
    }
}

#[derive(Clone)]
pub(super) struct KdbxFs {
    state: AppState,
    client_id: String,
}

impl KdbxFs {
    pub(super) fn new(state: AppState, client_id: String) -> Box<Self> {
        Box::new(Self { state, client_id })
    }
}

impl DavFileSystem for KdbxFs {
    fn open<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        let path_str = path.as_url_string();
        let state = self.state.clone();
        let client_id = self.client_id.clone();

        Box::pin(async move {
            if path_str.trim_matches('/') != DB_FILE {
                return Err(FsError::NotFound);
            }

            if options.write || options.create || options.create_new {
                Ok(Box::new(WriteFile {
                    buf: Vec::new(),
                    state,
                    client_id,
                }) as Box<dyn DavFile>)
            } else {
                {
                    let store = state.store.lock().await;
                    match store.merge_main_into_branch(client_id.clone()).await {
                        Ok(true) => {
                            state.notify_branches([&MAIN_BRANCH.to_string()]);
                        }
                        Ok(false) => {}
                        Err(e) => {
                            warn!(
                                "Client '{}': failed to merge main on read (serving stale data): {e:#}",
                                client_id
                            );
                        }
                    }
                }

                let config = Arc::clone(&state.config);
                let storage = {
                    let store = state.store.lock().await;
                    store
                        .read_branch(client_id.clone())
                        .await
                        .map_err(|e| {
                            warn!("Client '{}': failed to read branch: {e:#}", client_id);
                            FsError::GeneralFailure
                        })?
                        .ok_or(FsError::NotFound)?
                };

                let bytes = spawn_blocking(move || build_kdbx_sync(&storage, &config.database))
                    .await
                    .map_err(|_| FsError::GeneralFailure)?
                    .map_err(|e| {
                        warn!("Client '{}': failed to build KDBX: {e:#}", client_id);
                        FsError::GeneralFailure
                    })?;

                Ok(Box::new(ReadFile {
                    data: Bytes::from(bytes),
                    pos: 0,
                }) as Box<dyn DavFile>)
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        let path_str = path.as_url_string();
        Box::pin(async move {
            if path_str.trim_matches('/').is_empty() {
                let entry: Box<dyn DavDirEntry> = Box::new(DbFileEntry);
                let s = stream::once(futures_util::future::ready(Ok(entry)));
                Ok(Box::pin(s) as FsStream<Box<dyn DavDirEntry>>)
            } else {
                Err(FsError::NotFound)
            }
        })
    }

    fn metadata<'a>(
        &'a self,
        path: &'a dav_server::davpath::DavPath,
    ) -> FsFuture<'a, Box<dyn DavMetaData>> {
        let path_str = path.as_url_string();
        let state = self.state.clone();
        let client_id = self.client_id.clone();

        Box::pin(async move {
            let trimmed = path_str.trim_matches('/');

            if trimmed.is_empty() {
                return Ok(Box::new(DirMeta) as Box<dyn DavMetaData>);
            }

            if trimmed == DB_FILE {
                let exists = state
                    .store
                    .lock()
                    .await
                    .branch_tip_id(client_id.clone())
                    .await
                    .map_err(|_| FsError::GeneralFailure)?
                    .is_some();

                if exists {
                    Ok(Box::new(FileMeta {
                        len: 0,
                        modified: SystemTime::now(),
                    }) as Box<dyn DavMetaData>)
                } else {
                    Err(FsError::NotFound)
                }
            } else {
                Err(FsError::NotFound)
            }
        })
    }
}
