//! 只读发布本机 XLeRobot 模型。启动时按冻结清单装入一次，浏览器间共享网格。

use std::fs::File;
use std::io::Read;
use std::path::Path;

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

// dimos-deploy@9ece6bd 的 URDF 及它引用的全部网格，URDF 换行统一为 LF。
const MANIFEST: &str = include_str!("pose_assets/xlerobot.b3");
const DRIVE_WHEELS: &str = include_str!("pose_assets/drive-wheels.urdf.inc");
const WHEEL_ASSETS: [(&str, &[u8]); 2] = [
    (
        "meshes/omni-wheel.stl",
        include_bytes!("pose_assets/omni-wheel.stl"),
    ),
    (
        "meshes/omni-wheel.LICENSE",
        include_bytes!("pose_assets/omni-wheel.LICENSE"),
    ),
];
const MAX_FILE_BYTES: u64 = 1024 * 1024;

pub(super) fn router() -> Router {
    let dir = std::env::var_os("ROBOT_STATUS_URDF_DIR");
    model_router(dir.as_deref().map(Path::new), MANIFEST)
}

fn model_router(directory: Option<&Path>, manifest: &str) -> Router {
    let files: Vec<_> = manifest
        .lines()
        .map(|line| line.split_once("  ").expect("内嵌模型清单缺少文件名"))
        .collect();
    let load = |dir: &Path| -> Result<Vec<Bytes>, String> {
        let mut data = files
            .iter()
            .map(|(digest, name)| read_model_file(dir, name, digest))
            .collect::<Result<Vec<_>, _>>()?;
        let index = files
            .iter()
            .position(|(_, name)| *name == "xlerobot.urdf")
            .ok_or("上游清单缺少 xlerobot.urdf")?;
        let robot = std::str::from_utf8(&data[index])
            .map_err(|err| err.to_string())?
            .trim_end()
            .strip_suffix("</robot>")
            .ok_or("上游 URDF 缺少 robot 结束标签")?;
        data[index] = Bytes::from(format!("{robot}{DRIVE_WHEELS}</robot>\n"));
        data.extend(WHEEL_ASSETS.map(|(_, bytes)| Bytes::from_static(bytes)));
        Ok(data)
    };
    let loaded = directory.and_then(|dir| {
        load(dir)
            .inspect_err(|reason| tracing::warn!(%reason, "网页三维模型不可用"))
            .ok()
    });
    let mut router = Router::new();
    let paths = files
        .into_iter()
        .map(|(_, name)| name)
        .chain(WHEEL_ASSETS.map(|(path, _)| path));
    for (index, name) in paths.enumerate() {
        let data = loaded.as_ref().map(|items| items[index].clone());
        let content_type = match name.rsplit_once('.').map(|(_, ext)| ext) {
            Some("urdf") => "application/xml",
            Some("stl") => "model/stl",
            Some("LICENSE") => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        };
        // 上游摘要用于校验输入；HTTP 缓存按最终发送的字节校验。
        let etag = data
            .as_ref()
            .map_or_else(String::new, |bytes| format!("\"{}\"", blake3::hash(bytes)));
        // HTTP 路径只匹配清单中确切的文件名，请求内容不参与文件系统寻址。
        router = router.route(
            &format!("/assets/xlerobot/{name}"),
            get(move |headers: HeaderMap| {
                let response = asset_response(&headers, data.as_ref(), content_type, &etag);
                async move { response }
            }),
        );
    }
    router
}

fn read_model_file(dir: &Path, name: &str, digest: &str) -> Result<Bytes, String> {
    let path = dir.join(name);
    let io_error = |error: std::io::Error| format!("{name}: {error}");
    if !std::fs::metadata(&path).map_err(io_error)?.is_file() {
        return Err(format!("{name} 不是普通文件"));
    }
    let mut data = Vec::new();
    File::open(path)
        .map_err(io_error)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(io_error)?;
    if data.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("{name} 超出模型文件大小上限"));
    }
    if name == "xlerobot.urdf" && data.contains(&b'\r') {
        data = std::str::from_utf8(&data)
            .map_err(|error| error.to_string())?
            .replace("\r\n", "\n")
            .into_bytes();
    }
    if blake3::hash(&data).to_hex().as_str() != digest {
        return Err(format!("{name} 和网页模型版本不匹配"));
    }
    Ok(Bytes::from(data))
}

fn asset_response(
    headers: &HeaderMap,
    data: Option<&Bytes>,
    mime: &'static str,
    etag: &str,
) -> Response {
    let Some(data) = data else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "完整模型未就绪，请核对 ROBOT_STATUS_URDF_DIR 和模型版本。",
        )
            .into_response();
    };
    let cached = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|value| value == "*" || value.strip_prefix("W/").unwrap_or(value) == etag);
    let cache_headers = [
        (header::CACHE_CONTROL, "private, no-cache"),
        (header::ETAG, etag),
    ];
    if cached {
        (StatusCode::NOT_MODIFIED, cache_headers).into_response()
    } else {
        (cache_headers, [(header::CONTENT_TYPE, mime)], data.clone()).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    async fn check_wheel_bundle(app: &Router, unavailable: &Router, xml: &str) {
        let xml_path = "/assets/xlerobot/xlerobot.urdf";
        let get = |path: &str| Request::get(path).body(Body::empty()).unwrap();
        // 浏览器带旧模型的 ETag 首次访问新版本时，应收到三轮模型的完整正文。
        let previous_xml = Request::get(xml_path)
            .header(
                header::IF_NONE_MATCH,
                format!("\"{}\"", blake3::hash(xml.as_bytes())),
            )
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(previous_xml).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/xml");
        let tag = response.headers()[header::ETAG].clone();
        let bytes = to_bytes(response.into_body(), 16384).await.unwrap();
        assert_eq!(
            tag.to_str().unwrap(),
            format!("\"{}\"", blake3::hash(&bytes))
        );
        let model = std::str::from_utf8(&bytes).unwrap();
        assert!(model.starts_with("<robot name='demo'>\n<link name='base_link'/>\n"));
        assert!(model.ends_with("</robot>\n"));
        assert_eq!(model.matches("</robot>").count(), 1);
        assert_eq!(
            model.matches("filename=\"meshes/omni-wheel.stl\"").count(),
            3
        );
        assert_eq!(model.matches("type=\"fixed\"").count(), 3);
        for (path, bytes) in WHEEL_ASSETS {
            let url = format!("/assets/xlerobot/{path}");
            let response = app.clone().oneshot(get(&url)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mime = match path {
                "meshes/omni-wheel.stl" => "model/stl",
                "meshes/omni-wheel.LICENSE" => "text/plain; charset=utf-8",
                _ => panic!("未知的模型附加资源 {path}"),
            };
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
            assert_eq!(
                response.headers()[header::ETAG].to_str().unwrap(),
                format!("\"{}\"", blake3::hash(bytes))
            );
            assert_eq!(
                to_bytes(response.into_body(), bytes.len()).await.unwrap(),
                bytes
            );
            assert_eq!(
                unavailable
                    .clone()
                    .oneshot(get(&url))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
    }

    #[tokio::test]
    async fn serves_one_consistent_bundle_and_only_explicit_paths() {
        let dir = tempfile::tempdir().unwrap();
        let xml = "<robot name='demo'>\n<link name='base_link'/>\n</robot>\n";
        let mesh = b"mesh file";
        let manifest = format!(
            "{}  xlerobot.urdf\n{}  meshes/joint.stl\n",
            blake3::hash(xml.as_bytes()),
            blake3::hash(mesh)
        );
        std::fs::create_dir(dir.path().join("meshes")).unwrap();
        std::fs::write(dir.path().join("xlerobot.urdf"), xml.replace('\n', "\r\n")).unwrap();
        let mesh_path = dir.path().join("meshes/joint.stl");
        std::fs::write(&mesh_path, mesh).unwrap();
        let app = model_router(Some(dir.path()), &manifest);
        std::fs::write(mesh_path, b"changed").unwrap();
        let unavailable = model_router(Some(dir.path()), &manifest);
        let xml_path = "/assets/xlerobot/xlerobot.urdf";
        let get = |path: &str| Request::get(path).body(Body::empty()).unwrap();
        check_wheel_bundle(&app, &unavailable, xml).await;
        let mesh_path = "/assets/xlerobot/meshes/joint.stl";
        let response = app.clone().oneshot(get(mesh_path)).await.unwrap();
        assert_eq!(response.headers()[header::CONTENT_TYPE], "model/stl");
        let tag = response.headers()[header::ETAG].clone();
        assert_eq!(
            to_bytes(response.into_body(), 256).await.unwrap(),
            &mesh[..]
        );
        let revalidate = Request::get(mesh_path)
            .header(header::IF_NONE_MATCH, tag)
            .body(Body::empty())
            .unwrap();
        let cached = app.clone().oneshot(revalidate).await.unwrap();
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert!(to_bytes(cached.into_body(), 256).await.unwrap().is_empty());
        assert_eq!(
            app.oneshot(get("/assets/xlerobot/meshes/other.stl"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            unavailable.oneshot(get(xml_path)).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            model_router(None, &manifest)
                .oneshot(get(mesh_path))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
