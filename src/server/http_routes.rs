//! This module contains a parallel implementation of the /v2 HTTP api
//! routes for InfluxDB IOx based on the WriteBuffer storage implementation.
//!
//! The goal is that eventually the implementation in these routes
//! will replace the implementation in http_routes.rs
//!
//! Note that these routes are designed to be just helpers for now,
//! and "close enough" to the real /v2 api to be able to test InfluxDB IOx
//! without needing to create and manage a mapping layer from name -->
//! id (this is done by other services in the influx cloud)
//!
//! Long term, we expect to create IOx specific api in terms of
//! database names and may remove this quasi /v2 API from the Deloren.

use http::header::CONTENT_ENCODING;
use tracing::{debug, error, info};

use arrow_deps::arrow;
use influxdb_line_protocol::parse_lines;
use object_store;
use storage::{org_and_bucket_to_database, Database, DatabaseStore};
use data_types::partition_metadata::Partition;

use bytes::{Bytes, BytesMut};
use futures::{self, StreamExt};
use hyper::{Body, Method, StatusCode};
use serde::Deserialize;
use snafu::{OptionExt, ResultExt, Snafu};
use std::str;
use std::sync::{Arc, Mutex};
use std::io::{Write, Seek, SeekFrom, Cursor};
use arrow_deps::parquet::file::writer::TryClone;
use arrow_deps::parquet::arrow::ArrowWriter;

#[derive(Debug, Snafu)]
pub enum ApplicationError {
    // Internal (unexpected) errors
    #[snafu(display(
        "Internal error accessing org {}, bucket {}:  {}",
        org,
        bucket_name,
        source
    ))]
    BucketByName {
        org: String,
        bucket_name: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display(
        "Internal error writing points into org {}, bucket {}:  {}",
        org,
        bucket_name,
        source
    ))]
    WritingPoints {
        org: String,
        bucket_name: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display(
        "Internal error reading points from database {}:  {}",
        database,
        source
    ))]
    Query {
        database: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    // Application level errors
    #[snafu(display("Bucket {} not found in org {}", bucket, org))]
    BucketNotFound { org: String, bucket: String },

    #[snafu(display("Body exceeds limit of {} bytes", max_body_size))]
    RequestSizeExceeded { max_body_size: usize },

    #[snafu(display("Expected query string in request, but none was provided"))]
    ExpectedQueryString {},

    #[snafu(display("Invalid query string '{}': {}", query_string, source))]
    InvalidQueryString {
        query_string: String,
        source: serde_urlencoded::de::Error,
    },

    #[snafu(display("Query error: {}", source))]
    QueryError {
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Invalid request body '{}': {}", request_body, source))]
    InvalidRequestBody {
        request_body: String,
        source: serde_json::error::Error,
    },

    #[snafu(display("Invalid content encoding: {}", content_encoding))]
    InvalidContentEncoding { content_encoding: String },

    #[snafu(display("Error reading request header '{}' as Utf8: {}", header_name, source))]
    ReadingHeaderAsUtf8 {
        header_name: String,
        source: hyper::header::ToStrError,
    },

    #[snafu(display("Error reading request body: {}", source))]
    ReadingBody { source: hyper::error::Error },

    #[snafu(display("Error reading request body as utf8: {}", source))]
    ReadingBodyAsUtf8 { source: std::str::Utf8Error },

    #[snafu(display("Error parsing line protocol: {}", source))]
    ParsingLineProtocol {
        source: influxdb_line_protocol::Error,
    },

    #[snafu(display("Error decompressing body as gzip: {}", source))]
    ReadingBodyAsGzip { source: std::io::Error },

    #[snafu(display("No handler for {:?} {}", method, path))]
    RouteNotFound { method: Method, path: String },

    #[snafu(display("Internal error creating gzip decoder: {:?}", source))]
    CreatingGzipDecoder { source: std::io::Error },

    #[snafu(display(
        "Internal error from database {}:  {}",
        database,
        source
    ))]
    DatabaseError {
        database: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Error generating json response: {}", source))]
    JsonGenerationError{ source: serde_json::Error },
}

impl ApplicationError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::BucketByName { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::WritingPoints { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Query { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::QueryError { .. } => StatusCode::BAD_REQUEST,
            Self::BucketNotFound { .. } => StatusCode::NOT_FOUND,
            Self::RequestSizeExceeded { .. } => StatusCode::BAD_REQUEST,
            Self::ExpectedQueryString { .. } => StatusCode::BAD_REQUEST,
            Self::InvalidQueryString { .. } => StatusCode::BAD_REQUEST,
            Self::InvalidRequestBody { .. } => StatusCode::BAD_REQUEST,
            Self::InvalidContentEncoding { .. } => StatusCode::BAD_REQUEST,
            Self::ReadingHeaderAsUtf8 { .. } => StatusCode::BAD_REQUEST,
            Self::ReadingBody { .. } => StatusCode::BAD_REQUEST,
            Self::ReadingBodyAsUtf8 { .. } => StatusCode::BAD_REQUEST,
            Self::ParsingLineProtocol { .. } => StatusCode::BAD_REQUEST,
            Self::ReadingBodyAsGzip { .. } => StatusCode::BAD_REQUEST,
            Self::RouteNotFound { .. } => StatusCode::NOT_FOUND,
            Self::CreatingGzipDecoder { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::DatabaseError { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::JsonGenerationError { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

const MAX_SIZE: usize = 10_485_760; // max write request size of 10MB

#[derive(Debug, Deserialize)]
/// Body of the request to the /write endpoint
struct WriteInfo {
    org: String,
    bucket: String,
}

/// Parse the request's body into raw bytes, applying size limits and
/// content encoding as needed.
async fn parse_body(req: hyper::Request<Body>) -> Result<Bytes, ApplicationError> {
    // clippy says the const needs to be assigned to a local variable:
    // error: a `const` item with interior mutability should not be borrowed
    let header_name = CONTENT_ENCODING;
    let ungzip = match req.headers().get(&header_name) {
        None => false,
        Some(content_encoding) => {
            let content_encoding = content_encoding.to_str().context(ReadingHeaderAsUtf8 {
                header_name: header_name.as_str(),
            })?;
            match content_encoding {
                "gzip" => true,
                _ => InvalidContentEncoding { content_encoding }.fail()?,
            }
        }
    };

    let mut payload = req.into_body();

    let mut body = BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.expect("Should have been able to read the next chunk");
        // limit max size of in-memory payload
        if (body.len() + chunk.len()) > MAX_SIZE {
            return Err(ApplicationError::RequestSizeExceeded {
                max_body_size: MAX_SIZE,
            });
        }
        body.extend_from_slice(&chunk);
    }
    let body = body.freeze();

    // apply any content encoding needed
    if ungzip {
        use libflate::gzip::Decoder;
        use std::io::Read;
        let mut decoder = Decoder::new(&body[..]).context(CreatingGzipDecoder)?;
        // TODO cap the size of the decoded data (right
        // now this could decompress some crazy large
        // request)
        let mut decoded_data = Vec::new();
        decoder
            .read_to_end(&mut decoded_data)
            .context(ReadingBodyAsGzip)?;
        Ok(decoded_data.into())
    } else {
        Ok(body)
    }
}

#[tracing::instrument(level = "debug")]
async fn write<T: DatabaseStore>(
    req: hyper::Request<Body>,
    server: Arc<AppServer<T>>,
) -> Result<Option<Body>, ApplicationError> {
    let query = req.uri().query().context(ExpectedQueryString)?;

    let write_info: WriteInfo = serde_urlencoded::from_str(query).context(InvalidQueryString {
        query_string: String::from(query),
    })?;

    let db_name = org_and_bucket_to_database(&write_info.org, &write_info.bucket);

    let db = server
        .write_buffer
        .db_or_create(&db_name)
        .await
        .map_err(|e| Box::new(e) as _)
        .context(BucketByName {
            org: write_info.org.clone(),
            bucket_name: write_info.bucket.clone(),
        })?;

    let body = parse_body(req).await?;

    let body = str::from_utf8(&body).context(ReadingBodyAsUtf8)?;

    let lines = parse_lines(body)
        .collect::<Result<Vec<_>, influxdb_line_protocol::Error>>()
        .context(ParsingLineProtocol)?;

    debug!(
        "Inserting {} lines into database {} (org {} bucket {})",
        lines.len(),
        db_name,
        write_info.org,
        write_info.bucket
    );

    db.write_lines(&lines)
        .await
        .map_err(|e| Box::new(e) as _)
        .context(WritingPoints {
            org: write_info.org.clone(),
            bucket_name: write_info.bucket.clone(),
        })?;

    Ok(None)
}

#[derive(Deserialize, Debug)]
/// Body of the request to the /read endpoint
struct ReadInfo {
    org: String,
    bucket: String,
    // TODL This is currently a "SQL" request -- should be updated to conform
    // to the V2 API for reading (using timestamps, etc).
    sql_query: String,
}

// TODO: figure out how to stream read results out rather than rendering the whole thing in mem
#[tracing::instrument(level = "debug")]
async fn read<T: DatabaseStore>(
    req: hyper::Request<Body>,
    server: Arc<AppServer<T>>,
) -> Result<Option<Body>, ApplicationError> {
    let query = req.uri().query().context(ExpectedQueryString {})?;

    let read_info: ReadInfo = serde_urlencoded::from_str(query).context(InvalidQueryString {
        query_string: query,
    })?;

    let db_name = org_and_bucket_to_database(&read_info.org, &read_info.bucket);

    let db = server
        .write_buffer
        .db(&db_name)
        .await
        .context(BucketNotFound {
            org: read_info.org.clone(),
            bucket: read_info.bucket.clone(),
        })?;

    let results = db
        .query(&read_info.sql_query)
        .await
        .map_err(|e| Box::new(e) as _)
        .context(QueryError {})?;
    let results = arrow::util::pretty::pretty_format_batches(&results).unwrap();

    Ok(Some(results.into_bytes().into()))
}

// Route to test that the server is alive
#[tracing::instrument(level = "debug")]
async fn ping(req: hyper::Request<Body>) -> Result<Option<Body>, ApplicationError> {
    let response_body = "PONG";
    Ok(Some(response_body.into()))
}

fn no_op(name: &str) -> Result<Option<Body>, ApplicationError> {
    info!("NOOP: {}", name);
    Ok(None)
}

#[derive(Debug)]
pub struct AppServer<T> {
    pub write_buffer: Arc<T>,
    pub object_store: Arc<object_store::ObjectStore>,
}

#[derive(Deserialize, Debug)]
/// Arguments in the query string of the request to /partitions
struct DatabaseInfo {
    org: String,
    bucket: String,
}

#[tracing::instrument(level = "debug")]
async fn list_partitions<T: DatabaseStore>(
    req: hyper::Request<Body>,
    app_server: Arc<AppServer<T>>,
) -> Result<Option<Body>, ApplicationError> {
    let query = req.uri().query().context(ExpectedQueryString {})?;

    let info: DatabaseInfo = serde_urlencoded::from_str(query).context(InvalidQueryString {
        query_string: query,
    })?;

    let db_name = org_and_bucket_to_database(&info.org, &info.bucket);

    let db = app_server
        .write_buffer
        .db(&db_name)
        .await
        .context(BucketNotFound {
            org: &info.org,
            bucket: &info.bucket,
        })?;

    let partition_keys = db
        .partition_keys()
        .await
        .map_err(|e| Box::new(e) as _)
        .context(DatabaseError{database: &db_name})?;

    let result = serde_json::to_string(&partition_keys).context(JsonGenerationError)?;

    Ok(Some(result.into_bytes().into()))
}

#[derive(Deserialize, Debug)]
/// Arguments in the query string of the request to /snapshot
struct SnapshotInfo {
    org: String,
    bucket: String,
    partition: String,
}

#[tracing::instrument(level = "debug")]
async fn snapshot_partition<T: DatabaseStore>(
    req: hyper::Request<Body>,
    server: Arc<AppServer<T>>,
) -> Result<Option<Body>, ApplicationError> {
    let query = req.uri().query().context(ExpectedQueryString {})?;

    let snapshot: SnapshotInfo = serde_urlencoded::from_str(query).context(InvalidQueryString {
        query_string: query,
    })?;

    let db_name = org_and_bucket_to_database(&snapshot.org, &snapshot.bucket);

    let db = server
        .write_buffer
        .db(&db_name)
        .await
        .context(BucketNotFound {
            org: &snapshot.org,
            bucket: &snapshot.bucket,
        })?;

    // TODO: refactor this to happen in the background. Move this logic to the
    //       cluster (soon to be called server) package
    let tables = db
        .table_names_for_partition(&snapshot.partition)
        .await
        .map_err(|e| Box::new(e) as _)
        .context(DatabaseError{database: &db_name})?;

    let mut partition_meta = Partition::new(snapshot.partition.clone());

    for table in tables {
        let (batch, meta) = db
            .partition_table_to_arrow_with_meta(&table, &snapshot.partition)
            .await
            .map_err(|e| Box::new(e) as _)
            .context(DatabaseError{database: &db_name})?;


        partition_meta.tables.push(meta);

        let mem_writer = MemWriter::default();
        {
            let mut writer =
                ArrowWriter::try_new(mem_writer.clone(), batch.schema().clone(), None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        } // drop the reference to the MemWriter that the SerializedFileWriter has

        let data = mem_writer
            .into_inner()
            .expect("Nothing else should have a reference here");
        let len = data.len();
        let data = Bytes::from(data);
        let stream_data = std::io::Result::Ok(data);

        let table_path = format!("{}/data/{}/{}.parquet", db_name, &snapshot.partition, &table);

        server
            .object_store
            .put(
                &table_path,
                futures::stream::once(async move { stream_data }),
                len)
            .await
            .unwrap();
    }

    let meta_data_path = format!("{}/meta/{}.json", db_name, &snapshot.partition);
    let json_data = serde_json::to_vec(&partition_meta).context(JsonGenerationError)?;
    let data = Bytes::from(json_data.clone());
    let len = data.len();
    let stream_data = std::io::Result::Ok(data);
    server.object_store
        .put(
            &meta_data_path,
            futures::stream::once(async move { stream_data }),
            len,
        )
        .await
        .unwrap();

    Ok(Some(json_data.into()))
}

#[derive(Debug, Default, Clone)]
struct MemWriter {
    mem: Arc<Mutex<Cursor<Vec<u8>>>>,
}

impl MemWriter {
    /// Returns the inner buffer as long as there are no other references to the Arc.
    pub fn into_inner(self) -> Option<Vec<u8>> {
        Arc::try_unwrap(self.mem)
            .ok()
            .and_then(|mutex| mutex.into_inner().ok())
            .map(|cursor| cursor.into_inner())
    }
}

impl Write for MemWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.mem.lock().unwrap();
        inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut inner = self.mem.lock().unwrap();
        inner.flush()
    }
}

impl Seek for MemWriter {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let mut inner = self.mem.lock().unwrap();
        inner.seek(pos)
    }
}

impl TryClone for MemWriter {
    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            mem: self.mem.clone(),
        })
    }
}

pub async fn service<T: DatabaseStore>(
    req: hyper::Request<Body>,
    server: Arc<AppServer<T>>,
) -> http::Result<hyper::Response<Body>> {
    let method = req.method().clone();
    let uri = req.uri().clone();

    let response = match (req.method(), req.uri().path()) {
        (&Method::POST, "/api/v2/write") => write(req, server).await,
        (&Method::POST, "/api/v2/buckets") => no_op("create bucket"),
        (&Method::GET, "/ping") => ping(req).await,
        (&Method::GET, "/api/v2/read") => read(req, server).await,
        _ => Err(ApplicationError::RouteNotFound {
            method: method.clone(),
            path: uri.to_string(),
        }),
        // TODO: implement routing to change this API
        (&Method::GET, "/api/v1/partitions") => list_partitions(req, server).await,
        (&Method::GET, "/api/v1/snapshot") => snapshot_partition(req, server).await,
    };

    let result = match response {
        Ok(Some(body)) => hyper::Response::builder()
            .body(body)
            .expect("Should have been able to construct a response"),
        Ok(None) => hyper::Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("Should have been able to construct a response"),
        Err(e) => {
            error!(error = ?e, method = ?method, uri = ?uri, "Error while handing request");
            let json = serde_json::json!({"error": e.to_string()}).to_string();
            hyper::Response::builder()
                .status(e.status_code())
                .body(json.into())
                .expect("Should have been able to construct a response")
        }
    };
    info!(method = ?method, uri = ?uri, status = ?result.status(), "Handled request");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use http::header;
    use reqwest::{Client, Response};

    use hyper::service::{make_service_fn, service_fn};
    use hyper::Server;

    use storage::{test::TestDatabaseStore, DatabaseStore};
    use object_store::{ObjectStore, InMemory};

    type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
    type Result<T, E = Error> = std::result::Result<T, E>;

    #[tokio::test]
    async fn test_ping() -> Result<()> {
        let test_storage = Arc::new(AppServer{
            write_buffer: Arc::new(TestDatabaseStore::new()),
            object_store: Arc::new(ObjectStore::new_in_memory(InMemory::new())),
        });
        let server_url = test_server(test_storage.clone());

        let client = Client::new();
        let response = client.get(&format!("{}/ping", server_url)).send().await;

        // Print the response so if the test fails, we have a log of what went wrong
        check_response("ping", response, StatusCode::OK, "PONG").await;
        Ok(())
    }

    #[tokio::test]
    async fn test_write() -> Result<()> {
        let test_storage = Arc::new(AppServer{
            write_buffer: Arc::new(TestDatabaseStore::new()),
            object_store: Arc::new(ObjectStore::new_in_memory(InMemory::new())),
        });
        let server_url = test_server(test_storage.clone());

        let client = Client::new();

        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1568756160";

        // send write data
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                server_url, bucket_name, org_name
            ))
            .body(lp_data)
            .send()
            .await;

        check_response("write", response, StatusCode::NO_CONTENT, "").await;

        // Check that the data got into the right bucket
        let test_db = test_storage
            .write_buffer
            .db("MyOrg_MyBucket")
            .await
            .expect("Database exists");

        // Ensure the same line protocol data gets through
        assert_eq!(test_db.get_lines().await, vec![lp_data]);
        Ok(())
    }

    fn gzip_str(s: &str) -> Vec<u8> {
        use libflate::gzip::Encoder;
        use std::io::Write;

        let mut encoder = Encoder::new(Vec::new()).expect("creating gzip encoder");
        write!(encoder, "{}", s).expect("writing into encoder");
        encoder
            .finish()
            .into_result()
            .expect("successfully encoding gzip data")
    }

    #[tokio::test]
    async fn test_gzip_write() -> Result<()> {
        let test_storage = Arc::new(AppServer{
            write_buffer: Arc::new(TestDatabaseStore::new()),
            object_store: Arc::new(ObjectStore::new_in_memory(InMemory::new())),
        });
        let server_url = test_server(test_storage.clone());

        let client = Client::new();
        let lp_data = "h2o_temperature,location=santa_monica,state=CA surface_degrees=65.2,bottom_degrees=50.4 1568756160";

        // send write data encoded with gzip
        let bucket_name = "MyBucket";
        let org_name = "MyOrg";
        let response = client
            .post(&format!(
                "{}/api/v2/write?bucket={}&org={}",
                server_url, bucket_name, org_name
            ))
            .header(header::CONTENT_ENCODING, "gzip")
            .body(gzip_str(lp_data))
            .send()
            .await;

        check_response("write", response, StatusCode::NO_CONTENT, "").await;

        // Check that the data got into the right bucket
        let test_db = test_storage
            .write_buffer
            .db("MyOrg_MyBucket")
            .await
            .expect("Database exists");

        // Ensure the same line protocol data gets through
        assert_eq!(test_db.get_lines().await, vec![lp_data]);
        Ok(())
    }

    /// checks a http response against expected results
    async fn check_response(
        description: &str,
        response: Result<Response, reqwest::Error>,
        expected_status: StatusCode,
        expected_body: &str,
    ) {
        // Print the response so if the test fails, we have a log of
        // what went wrong
        println!("{} response: {:?}", description, response);

        if let Ok(response) = response {
            let status = response.status();
            let body = response
                .text()
                .await
                .expect("Converting request body to string");

            assert_eq!(status, expected_status);
            assert_eq!(body, expected_body);
        } else {
            panic!("Unexpected error response: {:?}", response);
        }
    }

    /// creates an instance of the http service backed by a in-memory
    /// testable database.  Returns the url of the server
    fn test_server(server: Arc<AppServer<TestDatabaseStore>>) -> String {
        let make_svc = make_service_fn(move |_conn| {
            let server = server.clone();
            async move {
                Ok::<_, http::Error>(service_fn(move |req| {
                    let server = server.clone();
                    super::service(req, server)
                }))
            }
        });

        // NB: specify port 0 to let the OS pick the port.
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
        let server = Server::bind(&bind_addr).serve(make_svc);
        let server_url = format!("http://{}", server.local_addr());
        tokio::task::spawn(server);
        println!("Started server at {}", server_url);
        server_url
    }
}
