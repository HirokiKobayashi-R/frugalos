use cannyls::lump::LumpId;
use cannyls_rpc::DeviceId;
use fibers_http_server::{Res, Status};
use httpcodec::{Header, HeaderField, HeaderFields};
use libfrugalos::entity::object::{FragmentsSummary, ObjectVersion};
use rustracing::carrier::IterHttpHeaderFields;
use trackable::error::ErrorKindExt;

use {Error, ErrorKind, Result};

#[derive(Debug)]
pub struct TraceHeader<'a>(pub Header<'a>);
impl<'a> IterHttpHeaderFields<'a> for TraceHeader<'a> {
    type Fields = TraceHeaderFields<'a>;

    fn fields(&'a self) -> Self::Fields {
        TraceHeaderFields(self.0.fields())
    }
}

#[derive(Debug)]
pub struct TraceHeaderFields<'a>(HeaderFields<'a>);
impl<'a> Iterator for TraceHeaderFields<'a> {
    type Item = (&'a str, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|f| (f.name(), f.value().as_bytes()))
    }
}

#[derive(Debug)]
pub struct ContentTypeJson;
impl From<ContentTypeJson> for HeaderField<'static, 'static> {
    fn from(_: ContentTypeJson) -> Self {
        unsafe { HeaderField::new_unchecked("Content-Type", "application/json") }
    }
}

#[derive(Debug)]
pub struct ContentTypeOctetStream;
impl From<ContentTypeOctetStream> for HeaderField<'static, 'static> {
    fn from(_: ContentTypeOctetStream) -> Self {
        unsafe { HeaderField::new_unchecked("Content-Type", "application/octet-stream") }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum HttpResult<T> {
    Ok(T),
    Err(Error),
}

pub fn make_json_response<T>(status: Status, body: Result<T>) -> Res<HttpResult<T>> {
    let body = match body {
        Err(e) => HttpResult::Err(e),
        Ok(v) => HttpResult::Ok(v),
    };
    let mut res = Res::new(status, body);
    res.header_mut().add_field(ContentTypeJson);
    res
}

pub fn make_object_response(
    status: Status,
    version: Option<ObjectVersion>,
    body: Result<Vec<u8>>,
) -> Res<HttpResult<Vec<u8>>> {
    // TODO `make_object_response` の利用箇所を `ObjectResponse:new` を使うように変更する
    ObjectResponse::new(status, body)
        .version(version)
        .into_response()
}

pub fn not_found() -> Error {
    ErrorKind::Other.cause("Not Found").into()
}

#[derive(Debug, Serialize)]
pub struct BucketStatistics {
    /// バケツ内のオブジェクト数.
    pub objects: u64,
}

#[derive(Debug)]
pub struct ObjectResponse {
    inner: Res<HttpResult<Vec<u8>>>,
}
impl ObjectResponse {
    /// `ObjectResponse` を生成する.
    pub fn new(status: Status, body: Result<Vec<u8>>) -> Self {
        Self {
            inner: match body {
                Err(e) => {
                    let mut res = Res::new(status, HttpResult::Err(e));
                    res.header_mut().add_field(ContentTypeJson);
                    res
                }
                Ok(content) => {
                    let mut res = Res::new(status, HttpResult::Ok(content));
                    res.header_mut().add_field(ContentTypeOctetStream);
                    res
                }
            },
        }
    }
    /// `ObjectVersion` をレスポンスにセットする.
    pub fn version(mut self, version: Option<ObjectVersion>) -> Self {
        if let Some(version) = version {
            self.inner.header_mut().add_field(unsafe {
                HeaderField::new_unchecked("ETag", &format!("\"{:x}\"", version.0))
            });
        }
        self
    }
    /// `FragmentsSummary` をレスポンスにセットする.
    pub fn fragments(mut self, fragments: Option<FragmentsSummary>) -> Self {
        if let Some(fragments) = fragments {
            let mut header = self.inner.header_mut();
            header.add_field(unsafe {
                HeaderField::new_unchecked(
                    "FrugalOS-Fragments-Corrupted",
                    &format!("{}", fragments.is_corrupted),
                )
            });
            header.add_field(unsafe {
                HeaderField::new_unchecked(
                    "FrugalOS-Fragments-Found-Total",
                    &format!("{}", fragments.found_total),
                )
            });
            header.add_field(unsafe {
                HeaderField::new_unchecked(
                    "FrugalOS-Fragments-Lost-Total",
                    &format!("{}", fragments.lost_total),
                )
            });
        }
        self
    }
    /// HTTP のレスポンスへと変換する.
    pub fn into_response(self) -> Res<HttpResult<Vec<u8>>> {
        self.inner
    }
}

#[derive(Serialize, Debug)]
pub struct DeleteFragmentResponse {
    pub version: String,
    /// 実際に削除が実行されたか
    pub deleted: bool,
    pub device_id: String,
    pub lump_id: String,
}

impl DeleteFragmentResponse {
    pub fn new(
        version: ObjectVersion,
        deleted: bool,
        device_id: DeviceId,
        lump_id: LumpId,
    ) -> Self {
        Self {
            version: format!("{:x}", version.0),
            deleted,
            device_id: device_id.into_string(),
            lump_id: lump_id.to_string(),
        }
    }
}
