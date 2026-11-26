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
    use crate::plugins::publication::{digest, fixtures::sign};
    use sha2::{Digest, Sha256};
    use std::io::Write;

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

    let publish = |name: &str, version: &str, extra_tool: bool, seed| {
        let package = packages.join(name);
        let mut manifest: Value =
            serde_json::from_slice(&std::fs::read(package.join("manifest.json")).unwrap()).unwrap();
        manifest["package_version"] = json!(version);
        if extra_tool {
            let mut tool = manifest["tools"][0].clone();
            tool["name"] = json!("count-lines");
            manifest["tools"].as_array_mut().unwrap().push(tool);
        }
        let (signed, _) = sign(
            &manifest,
            &std::fs::read(package.join("plugin")).unwrap(),
            None,
            seed,
        );
        let path = root.0.join(format!("{name}-{seed}.visor-plugin"));
        hypermesh_local_storage::open(&path, hypermesh_local_storage::Open::New)
            .unwrap()
            .write_all(&signed)
            .unwrap();
        (path, digest(&signed))
    };

    let author = PathBuf::from(std::env::var_os("VISOR_PLUGIN_PUBLICATION").unwrap());
    let author = body(
        request(
            &companion,
            "POST",
            "/plugins/inspect",
            json!({"source":author}),
        )
        .await,
    )
    .await;
    assert_eq!(author["publication"]["id"], "org.example.byte-count");
    assert_eq!(
        author["publication"]["publisher"]["verification"],
        "community"
    );

    let (current, current_digest) = publish("current", "0.1.0", false, 10);
    let preview = body(
        request(
            &companion,
            "POST",
            "/plugins/inspect",
            json!({"source":current}),
        )
        .await,
    )
    .await;
    assert_eq!(
        preview["publication"]["publisher"]["verification"],
        "community"
    );
    assert_eq!(preview["publication"]["scan_status"], "not_scanned");
    assert_eq!(preview["publication"]["publication_sha256"], current_digest);
    assert_eq!(preview["added_or_changed_tools"], json!(["count-bytes"]));
    assert!(preview["installed"].is_null());
    assert_eq!(
        request(&public, "GET", "/plugins", Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let receipt = request(
        &companion,
        "POST",
        "/plugins",
        json!({"source":current,"publication_sha256":current_digest,"expected_revision":null}),
    )
    .await;
    assert_eq!(receipt.status(), StatusCode::CREATED);
    let installed = body(receipt).await;
    let installed_url = "/plugins/org.example.byte-count";
    let activate = |entry: &Value, tool: &str| {
        json!({
            "plugin_id":"org.example.byte-count","revision":entry["revision"],"tool":tool,"target":target
        })
    };
    assert_eq!(
        request(&companion, "POST", &call_path, call(&target))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &companion,
            "POST",
            &list_path,
            activate(&installed, "count-bytes")
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    let result = body(request(&companion, "POST", &call_path, call(&target)).await).await;
    assert_eq!(result, json!({"result":{"byte_count":11}}));
    assert_eq!(admitted.load(Ordering::Acquire), 2);

    let (imposter, imposter_digest) = publish("slow", "0.1.1", true, 88);
    assert_eq!(
        request(&companion, "POST", "/plugins",
            json!({"source":imposter,"publication_sha256":imposter_digest,"expected_revision":installed["revision"]}))
            .await.status(),
        StatusCode::FORBIDDEN
    );
    let (update, update_digest) = publish("slow", "0.1.1", true, 10);
    let review = body(
        request(
            &companion,
            "POST",
            "/plugins/inspect",
            json!({"source":update}),
        )
        .await,
    )
    .await;
    assert_eq!(review["added_or_changed_tools"], json!(["count-lines"]));
    assert_eq!(review["installed"]["revision"], installed["revision"]);
    let updated = request(&companion, "POST", "/plugins",
        json!({"source":update,"publication_sha256":update_digest,"expected_revision":installed["revision"]})).await;
    assert_eq!(updated.status(), StatusCode::OK);
    let updated = body(updated).await;
    assert_eq!(
        request(&companion, "POST", &call_path, call(&target))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &companion,
            "POST",
            &list_path,
            activate(&installed, "count-bytes")
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &companion,
            "POST",
            &list_path,
            activate(&updated, "count-bytes")
        )
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
    assert_eq!(admitted.load(Ordering::Acquire), 3);
    let disabled = body(
        request(
            &companion,
            "PUT",
            installed_url,
            json!({"expected_revision":updated["revision"],"enabled":false}),
        )
        .await,
    )
    .await;
    assert_eq!(disabled["enabled"], false);
    let stopped = body(pending).await;
    assert_eq!(stopped["error"]["code"], "plugin_cancelled");
    // The blocking sandbox preparation may be finishing as the stream reports cancellation.
    tokio::time::timeout(Duration::from_secs(2), async {
        while std::fs::read_dir(&scratch).unwrap().next().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("staged executable was removed");
    assert_eq!(
        request(
            &companion,
            "POST",
            &list_path,
            activate(&disabled, "count-bytes")
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );

    let restored = body(
        request(
            &companion,
            "PUT",
            installed_url,
            json!({"expected_revision":disabled["revision"],"enabled":true}),
        )
        .await,
    )
    .await;
    assert_eq!(
        request(&companion, "POST", &call_path, call(&target))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &companion,
            "POST",
            &list_path,
            activate(&restored, "count-bytes")
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        request(
            &companion,
            "DELETE",
            installed_url,
            json!({"expected_revision":restored["revision"]})
        )
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
        body(request(&companion, "GET", "/plugins", Value::Null).await).await,
        json!({"plugins":[]})
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "CI supplies the signed GitHub issue plugin package"]
async fn github_issue_plugin_uses_the_selected_account_and_public_repository() {
    use crate::plugins::publication::digest;

    const TOKEN: &str = "github_pat_visor_contract_123456789";
    const PLUGIN: &str = "dev.visor.github-issue";
    let mode = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture_mode = Arc::clone(&mode);
    let fixture_calls = Arc::clone(&calls);
    let fixture = Router::new().fallback(get(move |uri: axum::http::Uri, headers: HeaderMap| {
        let current = fixture_mode.load(Ordering::Acquire);
        fixture_calls.fetch_add(1, Ordering::AcqRel);
        async move {
            let is_issue = uri.path().ends_with("/issues/7");
            assert_eq!(headers["x-github-api-version"], "2026-03-10");
            if is_issue {
                assert!(!headers.contains_key(header::AUTHORIZATION));
            } else {
                assert_eq!(headers[header::AUTHORIZATION], format!("Bearer {TOKEN}"));
            }
            let reply = |status, value| (status, Json(value));
            if current == 2 {
                return reply(StatusCode::SERVICE_UNAVAILABLE, json!({"message":TOKEN}));
            }
            if current == 3 {
                return reply(StatusCode::UNAUTHORIZED, json!({"message":TOKEN}));
            }
            match uri.path() {
                "/user" => reply(StatusCode::OK, json!({"id":21})),
                "/repos/Example/Project" => reply(StatusCode::OK, json!({
                    "full_name":"Example/Project", "private":current == 1,
                    "visibility":if current == 1 {"private"} else {"public"},
                })),
                "/repos/Example/Project/issues/7" if current == 4 => {
                    reply(StatusCode::NOT_FOUND, json!({"message":"repository just changed visibility"}))
                }
                "/repos/Example/Project/issues/7" => reply(StatusCode::OK, json!({
                    "id":1234, "number":7, "title":"Fixture issue", "body":TOKEN, "state":"open",
                    "repository_url":"https://api.github.com/repos/Example/Project",
                    "html_url":"https://github.com/Example/Project/issues/7",
                    "labels":[{"name":"excluded"}],
                })),
                _ => panic!("unexpected fixture endpoint"),
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });

    let root = Temp(std::env::temp_dir().join(format!("vgh-{}", Uuid::new_v4().simple())));
    hypermesh_local_storage::create_dir(&root.0).unwrap();
    let mut state = AppState::new(FakeDesktop::new(), Duration::from_millis(20));
    state.mcp_dir = root.0.clone();
    state.github_fixture = Some(address);
    let state = Arc::new(state);
    let companion = companion_router(state.clone());
    let regular = routes(state.clone(), false);
    let opened = request(
        &companion,
        "POST",
        "/session",
        json!({
            "purpose":"GitHub issue contract","recipes":[],"skills":[],
            "agent":{"name":"desk","instructions":""},
            "secrets":[{"name":crate::vault::GITHUB_ISSUES_SECRET,"source":"local","value":TOKEN}]
        }),
    )
    .await;
    assert_eq!(opened.status(), StatusCode::CREATED);
    let session: Uuid = serde_json::from_value(body(opened).await["id"].clone()).unwrap();
    lock(&state.automation)
        .control(Some(session), Control::Start)
        .unwrap();
    assert!(lock(&state.sessions)[&session]
        .vault()
        .get(crate::vault::GITHUB_ISSUES_SECRET)
        .is_none());

    let package = PathBuf::from(std::env::var_os("VISOR_GITHUB_ISSUE_PUBLICATION").unwrap());
    let installed = request(
        &companion,
        "POST",
        "/plugins",
        json!({
            "source":package, "publication_sha256":digest(&std::fs::read(&package).unwrap()),
            "expected_revision":null,
        }),
    )
    .await;
    assert_eq!(installed.status(), StatusCode::CREATED);
    let installed = body(installed).await;
    let target = json!({"account":21,"repository":"Example/Project"});
    let enable = json!({
        "target":target,"tool":"read-issue","plugin_id":PLUGIN,"revision":installed["revision"]
    });
    let grants = format!("/session/{session}/plugins");
    let plugin = format!("{grants}/{PLUGIN}");
    let read = format!("{plugin}/github-issue");
    assert_eq!(
        request(&companion, "POST", &grants, enable.clone())
            .await
            .status(),
        StatusCode::CREATED
    );
    let summary = body(request(&companion, "GET", &read, Value::Null).await).await;
    assert_eq!(summary["state"], "active");
    assert_eq!(summary["package_version"], "0.1.0");
    assert_eq!(summary["publisher"]["verification"], "community");
    assert_eq!(
        summary["access"],
        json!({"scope":"github.issue.read","target":target})
    );

    let call = json!({"target":target,"number":7});
    assert_eq!(
        request(&regular, "POST", &read, call.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let wrong = json!({"target":{"account":21,"repository":"Example/Other"},"number":7});
    assert_eq!(
        request(&companion, "POST", &read, wrong).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
    let result = body(request(&companion, "POST", &read, call.clone()).await).await;
    assert_eq!(
        result,
        json!({"result":{
            "id":1234,"number":7,"title":"Fixture issue","state":"open","body":"[redacted]",
            "url":"https://github.com/Example/Project/issues/7",
        }})
    );
    assert_eq!(calls.load(Ordering::Acquire), 3);

    mode.store(1, Ordering::Release);
    assert_eq!(
        request(&companion, "POST", &read, call.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::Acquire), 5);
    mode.store(4, Ordering::Release);
    assert_eq!(
        request(&companion, "POST", &read, call.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::Acquire), 8);

    mode.store(2, Ordering::Release);
    let unavailable = request(&companion, "POST", &read, call.clone()).await;
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body(unavailable).await["error"]["code"],
        "github_unavailable"
    );
    assert_eq!(
        request(&companion, "GET", "/plugins", Value::Null)
            .await
            .status(),
        StatusCode::OK
    );
    mode.store(0, Ordering::Release);
    assert_eq!(
        request(&companion, "POST", &read, call.clone())
            .await
            .status(),
        StatusCode::OK
    );

    mode.store(3, Ordering::Release);
    let revoked = request(&companion, "POST", &read, call.clone()).await;
    assert_eq!(revoked.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body(revoked).await["error"]["code"],
        "github_credential_revoked"
    );
    assert_eq!(
        body(request(&companion, "GET", &read, Value::Null).await).await["state"],
        "revoked"
    );
    let before = calls.load(Ordering::Acquire);
    assert_eq!(
        request(&companion, "POST", &read, call.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(calls.load(Ordering::Acquire), before);

    assert_eq!(
        request(&companion, "POST", &grants, enable).await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        request(&companion, "DELETE", &plugin, Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&companion, "POST", &read, call).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(calls.load(Ordering::Acquire), before);
    fixture.abort();
}
