//! Submit drafts through the local control socket and the existing forum reporter.
use super::*;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

const BODY_LIMIT: usize = 62 * 1024;
const REFUSED: &str = "Squad artifact publication refused or unconfirmed";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Submission {
    plane_task_id: Uuid,
    delivery_id: Uuid,
    artifact: Artifact,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", content = "draft", rename_all = "lowercase", deny_unknown_fields)]
enum Artifact {
    Brief(Value),
    Workbook(Value),
    Preview(Value),
}

impl Artifact {
    fn draft(&self) -> &Value {
        match self {
            Self::Brief(draft) | Self::Workbook(draft) | Self::Preview(draft) => draft,
        }
    }

    fn coordinate(&self, name: &str) -> Option<&str> {
        let value = self.draft()[name].as_str()?;
        Uuid::parse_str(value)
            .ok()
            .filter(|id| !id.is_nil() && id.to_string() == value)
            .map(|_| value)
    }

    fn valid(&self) -> bool {
        self.draft().get("signature").is_none()
            && ["taskId", "artifactId", "revisionId"].into_iter().all(|name| self.coordinate(name).is_some())
    }

    fn matches_reply(&self, reply: &Value) -> bool {
        reply.as_object().is_some_and(|fields| fields.len() == 4)
            && reply["version"] == 1
            && ["taskId", "artifactId", "revisionId"].into_iter().all(|name| {
                self.coordinate(name).is_some_and(|expected| reply[name].as_str() == Some(expected))
            })
    }
}

pub(super) struct Pending {
    session: Uuid,
    submission: Submission,
    reply: oneshot::Sender<Option<Value>>,
}

impl Pending {
    pub(super) fn matches(&self, session: Uuid, report: &Value, delivery: Uuid) -> bool {
        !self.reply.is_closed()
            && self.session == session
            && self.submission.delivery_id == delivery
            && report["status"] == "completed"
            && report["taskId"] == self.submission.plane_task_id.to_string()
    }

    pub(super) async fn publish<F>(
        mut self,
        report: Value,
        exchange: impl FnOnce(Value) -> F,
    ) -> bool
    where
        F: std::future::Future<Output = Option<Vec<u8>>>,
    {
        if self.reply.is_closed() {
            return false;
        }
        let envelope = json!({"version":1, "report":report, "artifact":self.submission.artifact});
        let response = tokio::select! {
            biased;
            _ = self.reply.closed() => return false,
            response = exchange(envelope) => response,
        };
        let reply = response.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|reply| self.submission.artifact.matches_reply(reply));
        let accepted = reply.is_some();
        let _ = self.reply.send(reply);
        accepted
    }
}

pub(super) fn route(state: Arc<AppState>, sender: mpsc::Sender<Pending>) -> Router {
    Router::new()
        .route("/session/:id/squad/artifacts", post(submit))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(state.clone(), managed_admission))
        .layer(Extension(sender))
        .with_state(state)
}

async fn submit(
    State(state): State<Arc<AppState>>,
    Extension(sender): Extension<mpsc::Sender<Pending>>,
    Path(session): Path<Uuid>,
    Json(submission): Json<Submission>,
) -> Result<Json<Value>, ApiError> {
    if submission.plane_task_id.is_nil() || submission.delivery_id.is_nil() || !submission.artifact.valid() {
        return Err(ApiError::bad("Invalid unsigned Squad artifact draft"));
    }
    if !state.has_session(session) {
        return Err(ApiError::missing());
    }
    let allowed = {
        let gate = lock(&state.automation);
        gate.phase() == Phase::Running && gate.session() == Some(session)
    };
    if !allowed {
        return Err(ApiError::conflict("Current companion consent required"));
    }
    let (reply, result) = oneshot::channel();
    sender.try_send(Pending { session, submission, reply })
        .map_err(|_| ApiError::unavailable(REFUSED))?;
    let response = tokio::time::timeout(Duration::from_secs(16), result).await
        .ok().and_then(Result::ok).flatten().ok_or_else(|| ApiError::conflict(REFUSED))?;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(kind: &str, extra: Value) -> (Uuid, Value, Value, Value) {
        let session = Uuid::new_v4();
        let plane = Uuid::new_v4();
        let delivery = Uuid::new_v4();
        let ids = json!({"taskId":Uuid::new_v4(), "artifactId":Uuid::new_v4(), "revisionId":Uuid::new_v4()});
        let mut draft = ids.clone();
        draft.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let request = json!({"planeTaskId":plane, "deliveryId":delivery, "artifact":{"kind":kind, "draft":draft}});
        let report = json!({"tenant":Uuid::new_v4(), "botId":Uuid::new_v4(), "assignmentId":Uuid::new_v4(), "sessionId":session,
            "taskId":plane, "deliveryId":delivery, "forumId":"forum_partner_fixture", "principal":"fixture", "status":"completed"});
        let mut reply = ids;
        reply["version"] = json!(1);
        (session, request, report, reply)
    }

    #[tokio::test]
    async fn publishes_each_exact_draft_and_reconciles_an_uncertain_reply() {
        for (kind, draft) in [
            ("brief", json!({"base":null, "markdown":"10 seats for 30 days"})),
            ("workbook", json!({"inputs":{"quantity":10,"unitPriceCents":12000,"discountBps":0}})),
            ("preview", json!({"base":null,"briefRevisionId":Uuid::new_v4(),"workbookRevisionId":Uuid::new_v4(),
                "copy":{"title":"Partner pilot","summary":"10 seats for 30 days","highlights":[],"nextStep":"Confirm the quote"}})),
        ] {
            let (session, request, report, expected) = fixture(kind, draft);
            let expected_frame = json!({"version":1, "report":report, "artifact":request["artifact"]});
            for response in [None, Some(expected.clone())] {
                let submission: Submission = serde_json::from_value(request.clone()).unwrap();
                assert!(submission.artifact.valid());
                let delivery = submission.delivery_id;
                let (reply, result) = oneshot::channel();
                let pending = Pending { session, submission, reply };
                assert!(pending.matches(session, &report, delivery));
                let accepted = pending.publish(report.clone(), |frame| {
                    assert_eq!(frame, expected_frame);
                    std::future::ready(response.as_ref().map(|value| serde_json::to_vec(value).unwrap()))
                }).await;
                assert_eq!(accepted, response.is_some());
                assert_eq!(result.await.unwrap(), response);
            }
        }
    }

    #[tokio::test]
    async fn refuses_a_different_delivery_or_revision_and_a_cancelled_caller() {
        let (session, request, report, mut wrong_reply) = fixture("brief", json!({"base":null, "markdown":"Draft"}));
        let submission: Submission = serde_json::from_value(request.clone()).unwrap();
        let delivery = submission.delivery_id;
        let (reply, result) = oneshot::channel();
        let pending = Pending { session, submission, reply };
        assert!(!pending.matches(Uuid::new_v4(), &report, delivery));
        assert!(!pending.matches(session, &report, Uuid::new_v4()));
        let mut wrong_task = report.clone();
        wrong_task["taskId"] = json!(Uuid::new_v4());
        assert!(!pending.matches(session, &wrong_task, delivery));
        wrong_task = report.clone();
        wrong_task["status"] = json!("failed");
        assert!(!pending.matches(session, &wrong_task, delivery));
        wrong_reply["revisionId"] = json!(Uuid::new_v4());
        assert!(!pending.publish(report.clone(), |_| std::future::ready(Some(serde_json::to_vec(&wrong_reply).unwrap()))).await);
        assert!(result.await.unwrap().is_none());

        let (reply, result) = oneshot::channel();
        drop(result);
        let pending = Pending { session, submission: serde_json::from_value(request).unwrap(), reply };
        assert!(!pending.matches(session, &report, delivery));
        assert!(!pending.publish(report, |_| -> std::future::Ready<Option<Vec<u8>>> {
            panic!("cancelled request must not reach the reporter")
        }).await);
    }
}
