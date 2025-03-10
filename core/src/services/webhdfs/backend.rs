// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use core::fmt::Debug;
use std::collections::HashMap;

use async_trait::async_trait;
use http::header::CONTENT_LENGTH;
use http::header::CONTENT_TYPE;
use http::Request;
use http::Response;
use http::StatusCode;
use log::debug;
use tokio::sync::OnceCell;

use super::error::parse_error;
use super::error::parse_error_msg;
use super::message::BooleanResp;
use super::message::DirectoryListingWrapper;
use super::message::FileStatusType;
use super::message::FileStatusWrapper;
use super::message::FileStatusesWrapper;
use super::pager::WebhdfsPager;
use super::writer::WebhdfsWriter;
use crate::raw::*;
use crate::*;

const WEBHDFS_DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9870";

/// [WebHDFS](https://hadoop.apache.org/docs/stable/hadoop-project-dist/hadoop-hdfs/WebHDFS.html)'s REST API support.
#[doc = include_str!("docs.md")]
#[derive(Default, Clone)]
pub struct WebhdfsBuilder {
    root: Option<String>,
    endpoint: Option<String>,
    delegation: Option<String>,
    disable_list_batch: bool,
}

impl Debug for WebhdfsBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("root", &self.root)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl WebhdfsBuilder {
    /// Set the working directory of this backend
    ///
    /// All operations will happen under this root
    ///
    /// # Note
    ///
    /// The root will be automatically created if not exists.
    pub fn root(&mut self, root: &str) -> &mut Self {
        if !root.is_empty() {
            self.root = Some(root.to_string())
        }

        self
    }

    /// Set the remote address of this backend
    /// default to `http://127.0.0.1:9870`
    ///
    /// Endpoints should be full uri, e.g.
    ///
    /// - `https://webhdfs.example.com:9870`
    /// - `http://192.168.66.88:9870`
    ///
    /// If user inputs endpoint without scheme, we will
    /// prepend `http://` to it.
    pub fn endpoint(&mut self, endpoint: &str) -> &mut Self {
        if !endpoint.is_empty() {
            // trim tailing slash so we can accept `http://127.0.0.1:9870/`
            self.endpoint = Some(endpoint.trim_end_matches('/').to_string());
        }
        self
    }

    /// Set the delegation token of this backend,
    /// used for authentication
    ///
    /// # Note
    /// The builder prefers using delegation token over username.
    /// If both are set, delegation token will be used.
    pub fn delegation(&mut self, delegation: &str) -> &mut Self {
        if !delegation.is_empty() {
            self.delegation = Some(delegation.to_string());
        }
        self
    }

    /// Disable batch listing
    ///
    /// # Note
    ///
    /// When listing a directory, the backend will default to use batch listing.
    /// If disable, the backend will list all files/directories in one request.
    pub fn disable_list_batch(&mut self) -> &mut Self {
        self.disable_list_batch = true;
        self
    }
}

impl Builder for WebhdfsBuilder {
    const SCHEME: Scheme = Scheme::Webhdfs;
    type Accessor = WebhdfsBackend;

    fn from_map(map: HashMap<String, String>) -> Self {
        let mut builder = WebhdfsBuilder::default();

        map.get("root").map(|v| builder.root(v));
        map.get("endpoint").map(|v| builder.endpoint(v));
        map.get("delegation").map(|v| builder.delegation(v));
        map.get("disable_list_batch")
            .filter(|v| v == &"true")
            .map(|_| builder.disable_list_batch());

        builder
    }

    /// build the backend
    ///
    /// # Note
    ///
    /// when building backend, the built backend will check if the root directory
    /// exits.
    /// if the directory does not exits, the directory will be automatically created
    fn build(&mut self) -> Result<Self::Accessor> {
        debug!("start building backend: {:?}", self);

        let root = normalize_root(&self.root.take().unwrap_or_default());
        debug!("backend use root {root}");

        // check scheme
        let endpoint = match self.endpoint.take() {
            Some(endpoint) => {
                if endpoint.starts_with("http") {
                    endpoint
                } else {
                    format!("http://{endpoint}")
                }
            }
            None => WEBHDFS_DEFAULT_ENDPOINT.to_string(),
        };
        debug!("backend use endpoint {}", endpoint);

        let auth = self
            .delegation
            .take()
            .map(|dt| format!("delegation_token={dt}"));

        let client = HttpClient::new()?;

        let backend = WebhdfsBackend {
            root,
            endpoint,
            auth,
            client,
            root_checker: OnceCell::new(),
            disable_list_batch: self.disable_list_batch,
        };

        Ok(backend)
    }
}

/// Backend for WebHDFS service
#[derive(Debug, Clone)]
pub struct WebhdfsBackend {
    root: String,
    endpoint: String,
    auth: Option<String>,
    root_checker: OnceCell<()>,

    pub disable_list_batch: bool,
    pub client: HttpClient,
}

impl WebhdfsBackend {
    /// create object or make a directory
    ///
    /// TODO: we should split it into mkdir and create
    pub fn webhdfs_create_object_request(
        &self,
        path: &str,
        size: Option<usize>,
        args: &OpWrite,
        body: AsyncBody,
    ) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);
        let op = if path.ends_with('/') {
            "MKDIRS"
        } else {
            "CREATE"
        };
        let mut url = format!(
            "{}/webhdfs/v1/{}?op={}&overwrite=true",
            self.endpoint,
            percent_encode_path(&p),
            op,
        );
        if let Some(auth) = &self.auth {
            url += format!("&{auth}").as_str();
        }

        let mut req = Request::put(&url);

        // mkdir does not redirect
        if path.ends_with('/') {
            return req.body(AsyncBody::Empty).map_err(new_request_build_error);
        }

        if let Some(size) = size {
            req = req.header(CONTENT_LENGTH, size.to_string());
        }
        if let Some(content_type) = args.content_type() {
            req = req.header(CONTENT_TYPE, content_type);
        }

        req.body(body).map_err(new_request_build_error)
    }

    async fn webhdfs_open_request(
        &self,
        path: &str,
        range: &BytesRange,
    ) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);
        let mut url = format!(
            "{}/webhdfs/v1/{}?op=OPEN",
            self.endpoint,
            percent_encode_path(&p),
        );
        if let Some(auth) = &self.auth {
            url += &format!("&{auth}");
        }

        if !range.is_full() {
            // Webhdfs does not support read from end
            if range.offset().is_none() && range.size().is_some() {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "webhdfs doesn't support read with suffix range",
                ));
            };

            if let Some(offset) = range.offset() {
                url += &format!("&offset={offset}");
            }
            if let Some(size) = range.size() {
                url += &format!("&length={size}")
            }
        }

        let req = Request::get(&url)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        Ok(req)
    }

    fn webhdfs_list_status_request(&self, path: &str) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);
        let mut url = format!(
            "{}/webhdfs/v1/{}?op=LISTSTATUS",
            self.endpoint,
            percent_encode_path(&p),
        );
        if let Some(auth) = &self.auth {
            url += format!("&{auth}").as_str();
        }

        let req = Request::get(&url)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;
        Ok(req)
    }

    pub(super) fn webhdfs_list_status_batch_request(
        &self,
        path: &str,
        args: &OpList,
    ) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        // if it's not the first time to call LISTSTATUS_BATCH, we will add &startAfter=<CHILD>
        let start_after_param = match args.start_after() {
            Some(sa) if sa.is_empty() => String::new(),
            Some(sa) => format!("&startAfter={}", sa),
            None => String::new(),
        };

        let mut url = format!(
            "{}/webhdfs/v1/{}?op=LISTSTATUS_BATCH{}",
            self.endpoint,
            percent_encode_path(&p),
            start_after_param
        );
        if let Some(auth) = &self.auth {
            url += format!("&{auth}").as_str();
        }

        let req = Request::get(&url)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;
        Ok(req)
    }

    async fn webhdfs_read_file(
        &self,
        path: &str,
        range: BytesRange,
    ) -> Result<Response<IncomingAsyncBody>> {
        let req = self.webhdfs_open_request(path, &range).await?;
        self.client.send(req).await
    }

    async fn webhdfs_get_file_status(&self, path: &str) -> Result<Response<IncomingAsyncBody>> {
        let p = build_abs_path(&self.root, path);
        let mut url = format!(
            "{}/webhdfs/v1/{}?op=GETFILESTATUS",
            self.endpoint,
            percent_encode_path(&p),
        );

        if let Some(auth) = &self.auth {
            url += format!("&{auth}").as_str();
        }

        let req = Request::get(&url)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        self.client.send(req).await
    }

    async fn webhdfs_delete(&self, path: &str) -> Result<Response<IncomingAsyncBody>> {
        let p = build_abs_path(&self.root, path);
        let mut url = format!(
            "{}/webhdfs/v1/{}?op=DELETE&recursive=false",
            self.endpoint,
            percent_encode_path(&p),
        );
        if let Some(auth) = &self.auth {
            url += format!("&{auth}").as_str();
        }

        let req = Request::delete(&url)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        self.client.send(req).await
    }

    async fn check_root(&self) -> Result<()> {
        let resp = self.webhdfs_get_file_status("/").await?;
        match resp.status() {
            StatusCode::OK => {
                let bs = resp.into_body().bytes().await?;

                let file_status = serde_json::from_slice::<FileStatusWrapper>(&bs)
                    .map_err(new_json_deserialize_error)?
                    .file_status;

                if file_status.ty == FileStatusType::File {
                    return Err(Error::new(
                        ErrorKind::ConfigInvalid,
                        "root path must be dir",
                    ));
                }
            }
            StatusCode::NOT_FOUND => {
                self.create_dir("/", OpCreateDir::new()).await?;
            }
            _ => return Err(parse_error(resp).await?),
        }
        Ok(())
    }
}

#[async_trait]
impl Accessor for WebhdfsBackend {
    type Reader = IncomingAsyncBody;
    type BlockingReader = ();
    type Writer = oio::OneShotWriter<WebhdfsWriter>;
    type BlockingWriter = ();
    type Pager = WebhdfsPager;
    type BlockingPager = ();

    fn info(&self) -> AccessorInfo {
        let mut am = AccessorInfo::default();
        am.set_scheme(Scheme::Webhdfs)
            .set_root(&self.root)
            .set_native_capability(Capability {
                stat: true,

                read: true,
                read_can_next: true,
                read_with_range: true,

                write: true,
                create_dir: true,
                delete: true,

                list: true,
                list_without_recursive: true,

                ..Default::default()
            });
        am
    }

    /// Create a file or directory
    async fn create_dir(&self, path: &str, _: OpCreateDir) -> Result<RpCreateDir> {
        let req = self.webhdfs_create_object_request(
            path,
            Some(0),
            &OpWrite::default(),
            AsyncBody::Empty,
        )?;

        let resp = self.client.send(req).await?;

        let status = resp.status();

        // WebHDFS's has a two-step create/append to prevent clients to send out
        // data before creating it.
        // According to the redirect policy of `reqwest` HTTP Client we are using,
        // the redirection should be done automatically.
        match status {
            StatusCode::CREATED | StatusCode::OK => {
                let bs = resp.into_body().bytes().await?;

                let resp = serde_json::from_slice::<BooleanResp>(&bs)
                    .map_err(new_json_deserialize_error)?;

                if resp.boolean {
                    Ok(RpCreateDir::default())
                } else {
                    Err(Error::new(
                        ErrorKind::Unexpected,
                        "webhdfs create dir failed",
                    ))
                }
            }
            _ => Err(parse_error(resp).await?),
        }
    }

    async fn read(&self, path: &str, args: OpRead) -> Result<(RpRead, Self::Reader)> {
        let range = args.range();
        let resp = self.webhdfs_read_file(path, range).await?;
        match resp.status() {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                let size = parse_content_length(resp.headers())?;
                Ok((RpRead::new().with_size(size), resp.into_body()))
            }
            // WebHDFS will returns 403 when range is outside of the end.
            StatusCode::FORBIDDEN => {
                let (parts, body) = resp.into_parts();
                let bs = body.bytes().await?;
                let s = String::from_utf8_lossy(&bs);
                if s.contains("out of the range") {
                    Ok((RpRead::new(), IncomingAsyncBody::empty()))
                } else {
                    Err(parse_error_msg(parts, &s)?)
                }
            }
            StatusCode::RANGE_NOT_SATISFIABLE => Ok((RpRead::new(), IncomingAsyncBody::empty())),
            _ => Err(parse_error(resp).await?),
        }
    }

    async fn write(&self, path: &str, args: OpWrite) -> Result<(RpWrite, Self::Writer)> {
        Ok((
            RpWrite::default(),
            oio::OneShotWriter::new(WebhdfsWriter::new(self.clone(), args, path.to_string())),
        ))
    }

    async fn stat(&self, path: &str, _: OpStat) -> Result<RpStat> {
        // if root exists and is a directory, stat will be ok
        self.root_checker
            .get_or_try_init(|| async { self.check_root().await })
            .await?;

        let resp = self.webhdfs_get_file_status(path).await?;
        let status = resp.status();
        match status {
            StatusCode::OK => {
                let bs = resp.into_body().bytes().await?;

                let file_status = serde_json::from_slice::<FileStatusWrapper>(&bs)
                    .map_err(new_json_deserialize_error)?
                    .file_status;

                let meta = match file_status.ty {
                    FileStatusType::Directory => Metadata::new(EntryMode::DIR),
                    FileStatusType::File => Metadata::new(EntryMode::FILE)
                        .with_content_length(file_status.length)
                        .with_last_modified(parse_datetime_from_from_timestamp_millis(
                            file_status.modification_time,
                        )?),
                };

                Ok(RpStat::new(meta))
            }

            _ => Err(parse_error(resp).await?),
        }
    }

    async fn delete(&self, path: &str, _: OpDelete) -> Result<RpDelete> {
        let resp = self.webhdfs_delete(path).await?;

        match resp.status() {
            StatusCode::OK => {
                resp.into_body().consume().await?;
                Ok(RpDelete::default())
            }
            _ => Err(parse_error(resp).await?),
        }
    }

    async fn list(&self, path: &str, args: OpList) -> Result<(RpList, Self::Pager)> {
        if args.recursive() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "WebHDFS doesn't support list with recursive",
            ));
        }

        let path = path.trim_end_matches('/');

        if !self.disable_list_batch {
            let req = self.webhdfs_list_status_batch_request(path, &OpList::default())?;
            let resp = self.client.send(req).await?;
            match resp.status() {
                StatusCode::OK => {
                    let bs = resp.into_body().bytes().await?;
                    let directory_listing = serde_json::from_slice::<DirectoryListingWrapper>(&bs)
                        .map_err(new_json_deserialize_error)?
                        .directory_listing;
                    let file_statuses = directory_listing.partial_listing.file_statuses.file_status;
                    let mut objects = WebhdfsPager::new(self.clone(), path, file_statuses);
                    objects.set_remaining_entries(directory_listing.remaining_entries);
                    Ok((RpList::default(), objects))
                }
                StatusCode::NOT_FOUND => {
                    let objects = WebhdfsPager::new(self.clone(), path, vec![]);
                    Ok((RpList::default(), objects))
                }
                _ => Err(parse_error(resp).await?),
            }
        } else {
            let req = self.webhdfs_list_status_request(path)?;
            let resp = self.client.send(req).await?;
            match resp.status() {
                StatusCode::OK => {
                    let bs = resp.into_body().bytes().await?;
                    let file_statuses = serde_json::from_slice::<FileStatusesWrapper>(&bs)
                        .map_err(new_json_deserialize_error)?
                        .file_statuses
                        .file_status;
                    let objects = WebhdfsPager::new(self.clone(), path, file_statuses);
                    Ok((RpList::default(), objects))
                }
                StatusCode::NOT_FOUND => {
                    let objects = WebhdfsPager::new(self.clone(), path, vec![]);
                    Ok((RpList::default(), objects))
                }
                _ => Err(parse_error(resp).await?),
            }
        }
    }
}
