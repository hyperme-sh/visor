use super::*;
use serde_json::{json, Value};
use tower::ServiceExt;

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn request(app: &Router, method: &str, path: &str, value: Value) -> Response {
    app.clone()
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body(response: Response) -> Value {
    let raw = tokio::time::timeout(Duration::from_secs(12), response.into_body().collect())
        .await
        .expect("contract deadline")
        .unwrap()
        .to_bytes();
    serde_json::from_slice(&raw).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "CI builds the separate plugin packages and provides VISOR_PLUGIN_CONTRACT_DIR"]
async fn independent_package_uses_owner_admission() {
    use crate::permission_templates::{Preferences, Preset, Scope, Selection};
    use sha2::{Digest, Sha256};

    let packages = PathBuf::from(std::env::var_os("VISOR_PLUGIN_CONTRACT_DIR").unwrap());
    let root = Temp(std::env::temp_dir().join(format!("vplug-{}", Uuid::new_v4().simple())));
    hypermesh_local_storage::create_dir(&root.0).unwrap();
    let project = root.0.join("project");
    std::fs::create_dir(&project).unwrap();
    let target = project.join("sample.txt");
    std::fs::write(&target, b"test input\n").unwrap();
    let socket = root.0.join("owner.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();

    let mut state = AppState::new(FakeDesktop::new(), Duration::from_millis(20));
    state.mcp_dir = root.0.clone();
    state.host_bridge = Some(crate::host_bridge::Bridge::plugin_contract(socket));
    let state = Arc::new(state);
    let companion = companion_router(state.clone());
    let public = routes(state.clone(), false);
    let created = request(&companion, "POST", "/session", json!({
        "purpose":"plugin contract","recipes":[],"skills":[],"agent":{"name":"desk","instructions":""}
    })).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = body(created).await;
    let session = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    lock(&state.automation)
        .control(Some(session), Control::Start)
        .unwrap();

    let organization = Uuid::new_v4();
    let device = Uuid::new_v4();
    let identity = json!({
        "tenant_id":Uuid::new_v4(),"owner_id":Uuid::new_v4(),"principal":"owner",
        "organization_id":organization,"device_id":device,"bot_id":Uuid::new_v4(),
        "assignment_id":Uuid::new_v4(),"session_id":session
    });
    let selection = Selection::new(
        Scope {
            organization,
            device,
            project: project.to_str().unwrap().into(),
        },
        Preset::LocalInspection,
        target.to_str().unwrap().into(),
    )
    .unwrap();
    let dir = local_reads::directory(&state).unwrap();
    let mut prefs = Preferences::empty();
    prefs.select(selection.clone()).unwrap();
    crate::permission_templates::save(&dir, &Preferences::empty(), &prefs).unwrap();

    let allow = Arc::new(AtomicBool::new(false));
    let reviewed = Arc::new(AtomicUsize::new(0));
    let admitted = Arc::new(AtomicUsize::new(0));
    let flags = (allow.clone(), reviewed.clone(), admitted.clone());
    let fixture_project = project.clone();
    let fixture_target = target.clone();
    let owner = tokio::spawn(async move {
        let revision = Uuid::new_v4();
        while let Ok((stream, _)) = listener.accept().await {
            let (allow, reviewed, admitted) = flags.clone();
            let (identity, project, target) = (
                identity.clone(),
                fixture_project.clone(),
                fixture_target.clone(),
            );
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let (allow, reviewed, admitted) =
                            (allow.clone(), reviewed.clone(), admitted.clone());
                        let (identity, project, target) =
                            (identity.clone(), project.clone(), target.clone());
                        async move {
                            let path = req.uri().path().to_owned();
                            let value = match path.as_str() {
                                "/machine-context" => {
                                    json!({"identity":identity,"workspace":project})
                                }
                                "/permission-read-review" | "/permission-read-admit" => {
                                    let req: Value = serde_json::from_slice(
                                        &req.into_body().collect().await.unwrap().to_bytes(),
                                    )
                                    .unwrap();
                                    assert_eq!(req["project"], json!(project));
                                    assert_eq!(req["action"]["operation"], "read");
                                    assert_eq!(req["action"]["target"], json!(target));
                                    assert_eq!(
                                        req["action"]["arguments_sha256"].as_str().unwrap().len(),
                                        64
                                    );
                                    let admit = path == "/permission-read-admit";
                                    if admit {
                                        assert_eq!(
                                            req["expected_policy_revision"],
                                            json!(revision)
                                        );
                                        admitted.fetch_add(1, Ordering::SeqCst);
                                    } else {
                                        reviewed.fetch_add(1, Ordering::SeqCst);
                                    }
                                    let allow = allow.load(Ordering::Acquire);
                                    json!({"version":1,"request_id":req["request_id"],"project":req["project"],
                                    "action":req["action"],"limits":req["action"]["limits"],"preview_only":false,
                                    "identity":identity,"permitted":allow,"decision":if allow {"allowed"} else {"deny"},
                                    "learning_eligible":false,"policy_revision":revision,
                                    "expires_at":crate::local_reads::now()+9.0,"dispatch_admitted":admit && allow})
                                }
                                _ => panic!("unexpected owner path {path}"),
                            };
                            Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(
                                Bytes::from(value.to_string()),
                            )))
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    let install = |name: &str| {
        let manifest = packages.join(name).join("manifest.json");
        let digest = format!("{:x}", Sha256::digest(std::fs::read(&manifest).unwrap()));
        json!({"manifest":manifest,"manifest_sha256":digest,"tool":"count-bytes","target":target})
    };
    let call = |path: &std::path::Path| {
        json!({
            "request_id":Uuid::new_v4(),"session_id":session,"selection":selection,
            "target":path,"max_bytes":1024,"timeout_seconds":8
        })
    };
    let list_path = format!("/session/{session}/plugins");
    let plugin_path = format!("{list_path}/org.example.byte-count");
    let call_path = format!("{plugin_path}/call");
    assert_eq!(
        request(&public, "POST", &list_path, install("current"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&companion, "POST", &list_path, install("current"))
            .await
            .status(),
        StatusCode::CREATED
    );
    let metadata = body(request(&companion, "GET", &list_path, Value::Null).await).await;
    assert_eq!(metadata["plugins"][0]["api_version"], 1);
    assert_eq!(metadata["plugins"][0]["tool"]["scope"], "workspace.read");
    assert_eq!(
        request(
            &companion,
            "POST",
            &call_path,
            call(&project.join("other.txt"))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(reviewed.load(Ordering::Acquire), 0);
    let refused = body(request(&companion, "POST", &call_path, call(&target)).await).await;
    assert_eq!(refused["error"]["code"], "read_refused");
    assert_eq!(admitted.load(Ordering::Acquire), 0);
    assert!(!root.0.join("plugin-scratch").exists());

    allow.store(true, Ordering::Release);
    let admit = body(request(&companion, "POST", &call_path, call(&target)).await).await;
    assert_eq!(admit, json!({"result":{"byte_count":11}}));
    assert_eq!(admitted.load(Ordering::Acquire), 1);

    assert_eq!(
        request(&companion, "DELETE", &plugin_path, Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&companion, "POST", &call_path, call(&target))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&companion, "POST", &list_path, install("slow"))
            .await
            .status(),
        StatusCode::CREATED
    );
    let pending = request(&companion, "POST", &call_path, call(&target)).await;
    let scratch = root.0.join("plugin-scratch");
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::fs::read_dir(&scratch)
            .ok()
            .and_then(|mut entries| entries.next())
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("slow package started");
    assert_eq!(admitted.load(Ordering::Acquire), 2);
    assert_eq!(
        request(&companion, "DELETE", &plugin_path, Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let stopped = body(pending).await;
    assert_eq!(stopped["error"]["code"], "plugin_cancelled");
    assert!(std::fs::read_dir(&scratch).unwrap().next().is_none());

    assert_eq!(
        request(&companion, "POST", &list_path, install("current"))
            .await
            .status(),
        StatusCode::CREATED
    );
    lock(&state.automation).pause();
    assert_eq!(
        request(&companion, "POST", &call_path, call(&target))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &companion,
            "DELETE",
            &format!("/session/{session}"),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert!(lock(&state.plugins).list(session).is_empty());
    owner.abort();
}
