//! A Jev that answers from a script, so the suite never needs a key, a network or a bill.
//!
//! The same bargain `MockDoor` makes for the model door: it satisfies the trait and gets no
//! private path through anything downstream, so a bug this hides is a bug in the live client
//! rather than in the route above it. It is compiled unconditionally — unlike the fixture
//! catalogue, which is feature-gated because it carries 65 KB of sample documents and a
//! filesystem verb into a binary that should not have them. This carries one scripted answer and
//! no capability at all, so there is nothing for a production build to be talked into.

use std::sync::{Arc, Mutex};

use super::{Answer, Ask, JevDoor, JevError, Judgement, Usage};

/// Replays one scripted judgement, or one scripted failure, and remembers what it was asked.
#[derive(Debug, Clone)]
pub struct MockJev {
    answers: Vec<(String, Answer)>,
    model: String,
    usage: Usage,
    request_id: Option<String>,
    fail_with: Option<JevError>,
    /// Every `Ask` this door was handed, in order — what a test asserts on to show that the
    /// question it wrote is the question that would have gone out.
    asked: Arc<Mutex<Vec<Ask>>>,
}

impl MockJev {
    /// A door that answers with exactly these, whatever it is asked.
    ///
    /// DELIBERATELY NOT DERIVED FROM THE QUESTION. A door that answered "yes" to any noul would
    /// make every test agree with itself: the interesting assertions are about a particular
    /// probability arriving intact at the other end of the route, and those need the number to be
    /// chosen by the test rather than by the door.
    #[must_use]
    pub fn answering(answers: impl IntoIterator<Item = (impl Into<String>, Answer)>) -> Self {
        Self {
            answers: answers
                .into_iter()
                .map(|(name, answer)| (name.into(), answer))
                .collect(),
            model: "jev-mock".to_string(),
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
            },
            request_id: Some("req_mock".to_string()),
            fail_with: None,
            asked: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A door that fails the same way every time. One constructor for all four failures, because
    /// the thing worth testing is that they stay four all the way out to the caller.
    #[must_use]
    pub fn failing_with(error: JevError) -> Self {
        let mut door = Self::answering(Vec::<(String, Answer)>::new());
        door.fail_with = Some(error);
        door
    }

    /// The token counts this door reports.
    #[must_use]
    pub fn with_usage(mut self, input_tokens: i64, output_tokens: i64) -> Self {
        self.usage = Usage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
        };
        self
    }

    /// The model name this door answers under.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// The request id this door reports — `None` for a service that sent none, which a caller
    /// must be able to survive.
    #[must_use]
    pub fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    /// What this door was asked, in order.
    #[must_use]
    pub fn asked(&self) -> Vec<Ask> {
        match self.asked.lock() {
            Ok(asked) => asked.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[async_trait::async_trait]
impl JevDoor for MockJev {
    async fn ask(&self, ask: Ask) -> Result<Judgement, JevError> {
        match self.asked.lock() {
            Ok(mut asked) => asked.push(ask),
            Err(poisoned) => poisoned.into_inner().push(ask),
        }
        if let Some(error) = &self.fail_with {
            return Err(error.clone());
        }
        Ok(Judgement {
            model: self.model.clone(),
            request_id: self.request_id.clone(),
            usage: self.usage.clone(),
            answers: self.answers.clone(),
        })
    }
}
