//! A local, single-use, fixed-purpose focus-window question. This is selection evidence,
//! never a successful execution result or permission to execute an action.
//!
//! The actor supplies a trusted *local* context and proposed refs. This module
//! renders the actual numbered question itself; it never extracts choices from
//! model prose. Call `commit_presented_text` only after that exact text was
//! presented. Feed every next owned text through `resolve`, or call `clear` on
//! cancellation/unrelated input. Normal plan validation, confirmation and
//! dispatch remain mandatory and are deliberately absent from this API.
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

const MAX_TTL: Duration = Duration::from_secs(120);
const MAX_WINDOWS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnStamp {
    pub generation: u64,
    pub session_epoch: u64,
    pub turn_id: u64,
}
impl TurnStamp {
    fn next(self) -> Option<Self> {
        Some(Self {
            turn_id: self.turn_id.checked_add(1)?,
            ..self
        })
    }
}

/// Caller/actor attestation about the original request, not model prose and
/// not permission to dispatch. A negated or non-focus request must never mint it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorIntentVerdict {
    AffirmativeFocus,
    NotFocusOrNegated,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ActorCheckedFocusIntent {
    source_turn: TurnStamp,
    source_id: String,
}
impl fmt::Debug for ActorCheckedFocusIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActorCheckedFocusIntent")
            .field("purpose", &"focus_window")
            .field("source_turn", &self.source_turn)
            .finish_non_exhaustive()
    }
}
impl ActorCheckedFocusIntent {
    /// The actor must independently check original owned intent first. This
    /// module does no intent NLU: e.g. never-close is NotFocusOrNegated, not a
    /// focus request merely because a model proposed a window reference.
    pub fn new(
        action_name: &str,
        verdict: ActorIntentVerdict,
        source_turn: TurnStamp,
        source_id: &str,
    ) -> Result<Self, ChoiceError> {
        if action_name != "focus_window" || verdict != ActorIntentVerdict::AffirmativeFocus {
            return Err(ChoiceError::PurposeMismatch);
        }
        let source_id = bounded(Some(source_id), 256)
            .ok_or(ChoiceError::PurposeMismatch)?
            .to_owned();
        Ok(Self {
            source_turn,
            source_id,
        })
    }
    pub fn source_turn(&self) -> TurnStamp {
        self.source_turn
    }
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionId(String);
impl QuestionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    fn fresh() -> Result<Self, ChoiceError> {
        let mut bytes = [0u8; 16];
        // IDs identify a question, not a capability. Still avoid collisions
        // across actors/restarts without encoding any native window token.
        let count = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
        if count != bytes.len() as isize {
            return Err(ChoiceError::Unavailable);
        }
        Ok(Self(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }
}

/// Model/UI question DTO: fixed focus purpose, no native token or WM identity.
#[derive(Debug, Serialize)]
pub struct QuestionDraft {
    question_id: String,
    question: String,
    purpose: &'static str,
}
impl QuestionDraft {
    pub fn id(&self) -> QuestionId {
        QuestionId(self.question_id.clone())
    }
    pub fn text(&self) -> &str {
        &self.question
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceError {
    NeedWindowNames,
    InvalidContext,
    Unavailable,
    NoPresentedQuestion,
    WrongQuestion,
    WrongTurn,
    Expired,
    ContextChanged,
    NotPureSelection,
    OutOfRange,
    UnverifiedAudioQuote,
    PurposeMismatch,
    AudioCorrelationMismatch,
}

/// Audio quotations cannot produce this module's owned-text selection proof.
/// A future acoustic adapter must retain that distinction explicitly.
pub enum SelectionInput<'a> {
    OwnedText(&'a str),
    UnverifiedAudioQuote(&'a str),
}

#[derive(Clone)]
struct WindowBinding {
    reference: String,
    token: String,
}
struct BoundQuestion {
    id: QuestionId,
    text: String,
    wm: String,
    turn: TurnStamp,
    created: Instant,
    windows: Vec<WindowBinding>,
    focus_intent: ActorCheckedFocusIntent,
}

/// Intentionally not Serialize/Clone; Debug never prints native identities.
pub struct SelectionProof {
    question: BoundQuestion,
    selected: usize,
    answer_turn: TurnStamp,
    expires: Instant,
    liveness: Weak<()>,
    created_at: Instant,
}
impl fmt::Debug for SelectionProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectionProof")
            .field("question_id", &self.question.id)
            .field("answer_turn", &self.answer_turn)
            .field("native_identity", &"[local only]")
            .finish()
    }
}
impl SelectionProof {
    /// Recheck only the selected identity's provenance, not an action approval.
    pub fn matches_focus_window(
        &self,
        action_name: &str,
        reference: &str,
        intent: &ActorCheckedFocusIntent,
        turn: TurnStamp,
        context: &Value,
        now: Instant,
    ) -> bool {
        action_name == "focus_window"
            && &self.question.focus_intent == intent
            && self.liveness.upgrade().is_some()
            && turn == self.answer_turn
            && now >= self.created_at
            && now >= self.question.created
            && now < self.expires
            && self.question.windows[self.selected].reference == reference
            && question_live(&self.question, context)
    }
    pub fn question_id(&self) -> &QuestionId {
        &self.question.id
    }
}

#[derive(Debug)]
pub struct SelectedWindow {
    reference: String,
    proof: SelectionProof,
}
impl SelectedWindow {
    pub fn window_ref(&self) -> &str {
        &self.reference
    }
    pub fn proof(&self) -> &SelectionProof {
        &self.proof
    }
}

/// Correlation copied by the actor from a currently owned audio item and the
/// fresh response-created/completed envelope, never from model argument fields.
/// No Serialize/Debug: this is local protocol metadata, not a model facts DTO.
pub struct AudioClaimScope<'a> {
    pub turn: TurnStamp,
    pub owned_audio_item_id: &'a str,
    pub review_response_id: &'a str,
}
struct AudioReviewBinding {
    question_id: QuestionId,
    turn: TurnStamp,
    owned_audio_item_id: String,
    review_response_id: String,
    started: Instant,
}

/// A model's NLU quote of audio, explicitly NOT text ownership, acoustic proof,
/// proof that the user heard a question, or an action/confirmation approval.
pub struct ModelAudioQuote {
    question_id: QuestionId,
    turn: TurnStamp,
    owned_audio_item_id: String,
    review_response_id: String,
    completed_at: Instant,
    quote: String,
}
impl ModelAudioQuote {
    pub fn from_actor_response(
        question_id: &QuestionId,
        scope: AudioClaimScope<'_>,
        quoted_audio: &str,
        completed_at: Instant,
    ) -> Result<Self, ChoiceError> {
        if bounded(Some(scope.owned_audio_item_id), 256).is_none()
            || bounded(Some(scope.review_response_id), 256).is_none()
            || quoted_audio.len() > 256
        {
            return Err(ChoiceError::AudioCorrelationMismatch);
        }
        Ok(Self {
            question_id: question_id.clone(),
            turn: scope.turn,
            owned_audio_item_id: scope.owned_audio_item_id.to_owned(),
            review_response_id: scope.review_response_id.to_owned(),
            completed_at,
            quote: quoted_audio.to_owned(),
        })
    }
}
impl fmt::Debug for ModelAudioQuote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelAudioQuote")
            .field("assurance", &"unverified_audio")
            .field("question_id", &self.question_id)
            .field("turn", &self.turn)
            .finish_non_exhaustive()
    }
}

/// Lower-assurance NLU selection constraint. There is intentionally no method
/// converting this claim to SelectionProof, OwnedText or an execution permit.
pub struct ModelAudioSelectionClaim {
    question: BoundQuestion,
    selected: usize,
    scope: AudioReviewBinding,
    expires: Instant,
    liveness: Weak<()>,
    created_at: Instant,
}
impl fmt::Debug for ModelAudioSelectionClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelAudioSelectionClaim")
            .field("assurance", &"unverified_audio")
            .field("question_id", &self.question.id)
            .field("turn", &self.scope.turn)
            .field("native_identity", &"[local only]")
            .finish_non_exhaustive()
    }
}
/// Optional model/telemetry projection of a *claim*. It carries no native
/// identity or approvals and must never be accepted back as local evidence.
#[derive(Debug, Serialize)]
pub struct ModelAudioClaimView<'a> {
    assurance: &'static str,
    purpose: &'static str,
    question_id: &'a str,
    window_ref: &'a str,
    action_approval: bool,
    acoustic_proof: bool,
    presented_audio_proof: bool,
}
impl ModelAudioSelectionClaim {
    pub fn assurance(&self) -> &'static str {
        "unverified_audio"
    }
    pub fn public_claim_view(&self) -> ModelAudioClaimView<'_> {
        ModelAudioClaimView {
            assurance: "unverified_audio",
            purpose: "focus_window",
            question_id: self.question.id.as_str(),
            window_ref: self.window_ref(),
            action_approval: false,
            acoustic_proof: false,
            presented_audio_proof: false,
        }
    }
    pub fn window_ref(&self) -> &str {
        &self.question.windows[self.selected].reference
    }
    pub fn question_id(&self) -> &QuestionId {
        &self.question.id
    }
    /// Only constrains a focus-window reference. Caller still independently
    /// validates original focus intent, model NLU risk, plan and confirmation.
    pub fn matches_focus_window(
        &self,
        action_name: &str,
        reference: &str,
        intent: &ActorCheckedFocusIntent,
        scope: AudioClaimScope<'_>,
        context: &Value,
        now: Instant,
    ) -> bool {
        action_name == "focus_window"
            && &self.question.focus_intent == intent
            && scope.turn == self.scope.turn
            && scope.owned_audio_item_id == self.scope.owned_audio_item_id
            && scope.review_response_id == self.scope.review_response_id
            && self.liveness.upgrade().is_some()
            && now >= self.scope.started
            && now >= self.created_at
            && now >= self.question.created
            && now < self.expires
            && self.window_ref() == reference
            && question_live(&self.question, context)
    }
}

pub struct WindowChoiceSlot {
    ttl: Duration,
    pending: Option<BoundQuestion>,
    latest: Option<BoundQuestion>,
    liveness: Arc<()>,
    actor_turn: Option<TurnStamp>,
    audio_review: Option<AudioReviewBinding>,
}
impl WindowChoiceSlot {
    pub fn new(ttl: Duration) -> Result<Self, ChoiceError> {
        if ttl.is_zero() || ttl > MAX_TTL {
            return Err(ChoiceError::Expired);
        }
        Ok(Self {
            ttl,
            pending: None,
            latest: None,
            liveness: Arc::new(()),
            actor_turn: None,
            audio_review: None,
        })
    }
    pub fn clear(&mut self) {
        self.pending = None;
        self.latest = None;
        self.liveness = Arc::new(());
        self.actor_turn = None;
        self.audio_review = None;
    }

    /// Must be called for every actor turn, including turns not sent to a model.
    pub fn on_turn(&mut self, turn: TurnStamp) {
        if self.actor_turn != Some(turn) {
            let expected = self
                .latest
                .as_ref()
                .is_some_and(|q| q.turn.next() == Some(turn))
                || self.pending.as_ref().is_some_and(|q| q.turn == turn);
            if !expected {
                self.clear();
            }
            self.actor_turn = Some(turn);
        }
    }

    /// Read-only presentation liveness; consumed selections remain governed by
    /// their separate proof/claim, never resurrected as an active question.
    pub fn has_live_presentation(
        &self,
        id: &QuestionId,
        turn: TurnStamp,
        context: &Value,
        now: Instant,
    ) -> bool {
        self.actor_turn == Some(turn)
            && self.latest.as_ref().is_some_and(|question| {
                &question.id == id
                    && (question.turn == turn || question.turn.next() == Some(turn))
                    && !expired(question.created, now, self.ttl)
                    && question_live(question, context)
            })
    }

    /// Proposed refs may originate anywhere. Only local lookup supplies tokens
    /// and labels. Their first-occurrence order is retained after deduplication.
    pub fn prepare_focus(
        &mut self,
        focus_intent: ActorCheckedFocusIntent,
        context: &Value,
        proposed_refs: &[&str],
        turn: TurnStamp,
        now: Instant,
    ) -> Result<QuestionDraft, ChoiceError> {
        self.clear();
        self.actor_turn = Some(turn);
        if focus_intent.source_turn.generation != turn.generation
            || focus_intent.source_turn.session_epoch != turn.session_epoch
            || focus_intent.source_turn.turn_id > turn.turn_id
        {
            return Err(ChoiceError::PurposeMismatch);
        }
        if proposed_refs.len() > 8 {
            return Err(ChoiceError::NeedWindowNames);
        }
        let wm = wm(context).ok_or(ChoiceError::InvalidContext)?.to_owned();
        let mut seen = BTreeSet::new();
        let refs: Vec<_> = proposed_refs
            .iter()
            .copied()
            .filter(|r| seen.insert(*r))
            .collect();
        if !(2..=3).contains(&refs.len()) {
            return Err(ChoiceError::NeedWindowNames);
        }
        let mut labels = BTreeSet::new();
        let mut windows = Vec::new();
        let mut choices = Vec::new();
        for (index, reference) in refs.iter().enumerate() {
            let window = local_window(context, reference).ok_or(ChoiceError::InvalidContext)?;
            let token =
                bounded(window["target_token"].as_str(), 256).ok_or(ChoiceError::InvalidContext)?;
            if windows.iter().any(|w: &WindowBinding| w.token == token) {
                return Err(ChoiceError::InvalidContext);
            }
            let sensitive = context["windows"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|w| bounded(w["target_token"].as_str(), 256))
                .chain(std::iter::once(wm.as_str()));
            for secret in sensitive {
                if ["class", "title"]
                    .iter()
                    .any(|key| window[*key].as_str().is_some_and(|v| v.contains(secret)))
                {
                    return Err(ChoiceError::NeedWindowNames);
                }
            }
            let label = label(window).ok_or(ChoiceError::NeedWindowNames)?;
            if !labels.insert(label.to_lowercase()) {
                return Err(ChoiceError::NeedWindowNames);
            }
            choices.push(format!(
                "{}. {}",
                index + 1,
                serde_json::to_string(&label).unwrap()
            ));
            windows.push(WindowBinding {
                reference: (*reference).to_owned(),
                token: token.to_owned(),
            });
        }
        let text = format!("要切换/聚焦哪个窗口？{}。", choices.join("；"));
        if text.chars().count() > 160 {
            return Err(ChoiceError::NeedWindowNames);
        }
        let id = QuestionId::fresh()?;
        let draft = QuestionDraft {
            question_id: id.0.clone(),
            question: text.clone(),
            purpose: "focus_window",
        };
        self.pending = Some(BoundQuestion {
            id,
            text,
            wm,
            turn,
            created: now,
            windows,
            focus_intent,
        });
        Ok(draft)
    }

    /// The caller attests actual canonical *text* presentation, not an audio
    /// transcript, successful action, approval or arbitrary assistant message.
    /// This is not a certificate of pixels being read, audio playback or hearing.
    pub fn commit_presented_text(
        &mut self,
        id: &QuestionId,
        emitted_text: &str,
        turn: TurnStamp,
        context: &Value,
        now: Instant,
    ) -> Result<(), ChoiceError> {
        let mut question = self
            .pending
            .take()
            .ok_or(ChoiceError::NoPresentedQuestion)?;
        if &question.id != id || question.text != emitted_text {
            return Err(ChoiceError::WrongQuestion);
        }
        if question.turn != turn {
            return Err(ChoiceError::WrongTurn);
        }
        if expired(question.created, now, self.ttl) {
            return Err(ChoiceError::Expired);
        }
        if !question_live(&question, context) {
            return Err(ChoiceError::ContextChanged);
        }
        question.created = now;
        self.latest = Some(question);
        Ok(())
    }

    /// Register the actor's current owned item + response-created correlation.
    /// This does not assert audio delivery or interpret speech. A replacement
    /// registration makes an older response's quote unusable.
    pub fn bind_audio_review(
        &mut self,
        id: &QuestionId,
        scope: AudioClaimScope<'_>,
        context: &Value,
        now: Instant,
    ) -> Result<(), ChoiceError> {
        let question = self
            .latest
            .as_ref()
            .ok_or(ChoiceError::NoPresentedQuestion)?;
        if &question.id != id {
            return Err(ChoiceError::WrongQuestion);
        }
        if question.turn.next() != Some(scope.turn) {
            return Err(ChoiceError::WrongTurn);
        }
        if expired(question.created, now, self.ttl) {
            return Err(ChoiceError::Expired);
        }
        if !question_live(question, context) {
            return Err(ChoiceError::ContextChanged);
        }
        if bounded(Some(scope.owned_audio_item_id), 256).is_none()
            || bounded(Some(scope.review_response_id), 256).is_none()
        {
            return Err(ChoiceError::AudioCorrelationMismatch);
        }
        self.actor_turn = Some(scope.turn);
        self.audio_review = Some(AudioReviewBinding {
            question_id: id.clone(),
            turn: scope.turn,
            owned_audio_item_id: scope.owned_audio_item_id.to_owned(),
            review_response_id: scope.review_response_id.to_owned(),
            started: now,
        });
        Ok(())
    }

    /// Consume a correlated model quote as low-assurance NLU only. Never calls
    /// the owned-text path and never constructs a SelectionProof.
    pub fn resolve_model_audio_claim(
        &mut self,
        quote: ModelAudioQuote,
        context: &Value,
        now: Instant,
    ) -> Result<ModelAudioSelectionClaim, ChoiceError> {
        let question = self
            .latest
            .as_ref()
            .ok_or(ChoiceError::NoPresentedQuestion)?;
        if question.id != quote.question_id {
            return Err(ChoiceError::WrongQuestion);
        }
        let binding = self
            .audio_review
            .as_ref()
            .ok_or(ChoiceError::AudioCorrelationMismatch)?;
        if binding.question_id != quote.question_id
            || binding.turn != quote.turn
            || binding.owned_audio_item_id != quote.owned_audio_item_id
            || binding.review_response_id != quote.review_response_id
            || quote.completed_at < binding.started
            || quote.completed_at > now
        {
            return Err(ChoiceError::AudioCorrelationMismatch);
        }
        if question.turn.next() != Some(quote.turn) {
            return Err(ChoiceError::WrongTurn);
        }
        if expired(question.created, now, self.ttl) {
            self.clear();
            return Err(ChoiceError::Expired);
        }
        if !question_live(question, context) {
            self.clear();
            return Err(ChoiceError::ContextChanged);
        }
        let selected = match selector(&quote.quote) {
            Some(index) => index,
            None => {
                self.clear();
                return Err(ChoiceError::NotPureSelection);
            }
        };
        if selected >= question.windows.len() {
            self.clear();
            return Err(ChoiceError::OutOfRange);
        }
        let expires = question
            .created
            .checked_add(self.ttl)
            .ok_or(ChoiceError::Expired)?;
        Ok(ModelAudioSelectionClaim {
            question: self.latest.take().unwrap(),
            selected,
            scope: self.audio_review.take().unwrap(),
            expires,
            liveness: Arc::downgrade(&self.liveness),
            created_at: now,
        })
    }

    /// Only a full, pure selector in the next turn can consume the latest slot.
    /// A non-selector retires the question; it never becomes a new instruction.
    pub fn resolve(
        &mut self,
        id: &QuestionId,
        turn: TurnStamp,
        input: SelectionInput<'_>,
        context: &Value,
        now: Instant,
    ) -> Result<SelectedWindow, ChoiceError> {
        let latest = self
            .latest
            .as_ref()
            .ok_or(ChoiceError::NoPresentedQuestion)?;
        if &latest.id != id {
            return Err(ChoiceError::WrongQuestion);
        }
        let question = self.latest.take().unwrap();
        self.audio_review = None;
        self.actor_turn = Some(turn);
        if question.turn.next() != Some(turn) {
            return Err(ChoiceError::WrongTurn);
        }
        if expired(question.created, now, self.ttl) {
            return Err(ChoiceError::Expired);
        }
        if !question_live(&question, context) {
            return Err(ChoiceError::ContextChanged);
        }
        let text = match input {
            SelectionInput::OwnedText(text) => text,
            SelectionInput::UnverifiedAudioQuote(_) => {
                return Err(ChoiceError::UnverifiedAudioQuote);
            }
        };
        let selected = selector(text).ok_or(ChoiceError::NotPureSelection)?;
        let reference = question
            .windows
            .get(selected)
            .ok_or(ChoiceError::OutOfRange)?
            .reference
            .clone();
        let expires = question
            .created
            .checked_add(self.ttl)
            .ok_or(ChoiceError::Expired)?;
        Ok(SelectedWindow {
            reference,
            proof: SelectionProof {
                question,
                selected,
                answer_turn: turn,
                expires,
                liveness: Arc::downgrade(&self.liveness),
                created_at: now,
            },
        })
    }
}

fn expired(created: Instant, now: Instant, ttl: Duration) -> bool {
    now.checked_duration_since(created)
        .is_none_or(|age| age >= ttl)
}
fn bounded(value: Option<&str>, max: usize) -> Option<&str> {
    value.filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
}
fn wm(context: &Value) -> Option<&str> {
    bounded(context["wm_instance_id"].as_str(), 256)
}
fn local_window<'a>(context: &'a Value, reference: &str) -> Option<&'a Value> {
    if !reference.starts_with('w')
        || reference.len() > 64
        || !reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return None;
    }
    let windows = context["windows"]
        .as_array()
        .filter(|w| w.len() <= MAX_WINDOWS)?;
    let mut matches = windows
        .iter()
        .filter(|w| w["window_ref"].as_str() == Some(reference));
    let found = matches.next()?;
    let token = bounded(found["target_token"].as_str(), 256)?;
    (matches.next().is_none()
        && found["closed"] != true
        && found["live"] != false
        && windows
            .iter()
            .filter(|w| w["target_token"].as_str() == Some(token))
            .count()
            == 1)
        .then_some(found)
}
fn question_live(question: &BoundQuestion, context: &Value) -> bool {
    wm(context) == Some(question.wm.as_str())
        && question.windows.iter().all(|expected| {
            local_window(context, &expected.reference).is_some_and(|window| {
                window["target_token"].as_str() == Some(expected.token.as_str())
            })
        })
}
fn label(window: &Value) -> Option<String> {
    // The feedback publisher strips Arabic Letter Mark; accepting it here
    // would display a different question than the exact committed text.
    if ["class", "title"].iter().any(|field| {
        window[*field]
            .as_str()
            .is_some_and(|text| text.contains('\u{061c}'))
    }) {
        return None;
    }
    let clean = |value: &Value| -> Option<String> {
        let text = value.as_str()?;
        if text.chars().any(|c| c.is_control() || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')) { return None; }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!text.is_empty()
            && !["unknown", "untitled", "未知", "无标题", "未命名"]
                .contains(&text.to_lowercase().as_str()))
        .then_some(text)
    };
    let class = clean(&window["class"]);
    let title = clean(&window["title"]);
    let value = match (class, title) {
        (Some(class), Some(title)) if class.to_lowercase() != title.to_lowercase() => {
            format!("{} · {}", class.chars().take(10).collect::<String>(), title)
        }
        (Some(value), _) | (_, Some(value)) => value,
        _ => return None,
    };
    Some(value.chars().take(32).collect())
}
fn selector(text: &str) -> Option<usize> {
    match text.trim().to_lowercase().as_str() {
        "1" | "一" | "第一个" | "第1个" | "first" | "the first" | "first one" => Some(0),
        "2" | "二" | "第二个" | "第2个" | "second" | "the second" | "second one" => Some(1),
        "3" | "三" | "第三个" | "第3个" | "third" | "the third" | "third one" => Some(2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn turn(id: u64) -> TurnStamp {
        TurnStamp {
            generation: 4,
            session_epoch: 7,
            turn_id: id,
        }
    }
    fn context() -> Value {
        json!({"wm_instance_id":"wm-1","windows":[
        {"window_ref":"w1","target_token":"native-secret-1","class":"Editor","title":"Project"},
        {"window_ref":"w2","target_token":"native-secret-2","class":"Browser","title":"Documents"},
        {"window_ref":"w3","target_token":"native-secret-3","class":"Browser","title":"Mail"}]})
    }
    fn focus_intent() -> ActorCheckedFocusIntent {
        ActorCheckedFocusIntent::new(
            "focus_window",
            ActorIntentVerdict::AffirmativeFocus,
            turn(10),
            "owned-focus-request",
        )
        .unwrap()
    }
    fn slot() -> WindowChoiceSlot {
        WindowChoiceSlot::new(Duration::from_secs(10)).unwrap()
    }
    fn presented(
        slot: &mut WindowChoiceSlot,
        context: &Value,
        refs: &[&str],
        now: Instant,
    ) -> QuestionDraft {
        let draft = slot
            .prepare_focus(focus_intent(), context, refs, turn(10), now)
            .unwrap();
        slot.commit_presented_text(&draft.id(), draft.text(), turn(10), context, now)
            .unwrap();
        draft
    }
    #[test]
    fn read_only_presentation_liveness_expires_and_never_resurrects_consumed_proofs() {
        let ctx = context();
        let now = Instant::now();
        let mut slot = slot();
        let draft = presented(&mut slot, &ctx, &["w2", "w3"], now);
        assert!(slot.has_live_presentation(&draft.id(), turn(10), &ctx, now));
        assert!(!slot.has_live_presentation(
            &draft.id(),
            turn(10),
            &ctx,
            now + Duration::from_secs(11)
        ));
        slot.on_turn(turn(11));
        assert!(slot.has_live_presentation(&draft.id(), turn(11), &ctx, now));
        let selected = slot
            .resolve(
                &draft.id(),
                turn(11),
                SelectionInput::OwnedText("第一个"),
                &ctx,
                now,
            )
            .unwrap();
        assert!(!slot.has_live_presentation(&draft.id(), turn(11), &ctx, now));
        assert!(selected.proof().matches_focus_window(
            "focus_window",
            "w2",
            &focus_intent(),
            turn(11),
            &ctx,
            now
        ));
        slot.on_turn(turn(12));
        assert!(!selected.proof().matches_focus_window(
            "focus_window",
            "w2",
            &focus_intent(),
            turn(11),
            &ctx,
            now
        ));
    }
    #[test]
    fn reversed_proposal_order_is_the_actual_question_order_and_only_selects_identity() {
        let mut slot = slot();
        let now = Instant::now();
        let context = context();
        let draft = presented(&mut slot, &context, &["w3", "w2", "w3"], now);
        assert!(draft.text().find("Mail").unwrap() < draft.text().find("Documents").unwrap());
        let public = serde_json::to_string(&draft).unwrap();
        assert!(
            !public.contains("native-secret") && !public.contains("wm-1") && !public.contains("w3")
        );
        let selected = slot
            .resolve(
                &draft.id(),
                turn(11),
                SelectionInput::OwnedText("第二个"),
                &context,
                now,
            )
            .unwrap();
        assert_eq!(selected.window_ref(), "w2");
        assert!(selected.proof().matches_focus_window(
            "focus_window",
            "w2",
            &focus_intent(),
            turn(11),
            &context,
            now
        ));
        assert!(!selected.proof().matches_focus_window(
            "focus_window",
            "w3",
            &focus_intent(),
            turn(11),
            &context,
            now
        ));
        assert!(!format!("{selected:?}").contains("native-secret"));
        assert_eq!(
            slot.resolve(
                &draft.id(),
                turn(11),
                SelectionInput::OwnedText("1"),
                &context,
                now
            )
            .unwrap_err(),
            ChoiceError::NoPresentedQuestion
        );
    }
    #[test]
    fn draft_is_not_a_presented_question_and_noncanonical_text_cannot_commit() {
        let mut slot = slot();
        let now = Instant::now();
        let context = context();
        let draft = slot
            .prepare_focus(focus_intent(), &context, &["w2", "w3"], turn(10), now)
            .unwrap();
        assert_eq!(
            slot.resolve(
                &draft.id(),
                turn(11),
                SelectionInput::OwnedText("1"),
                &context,
                now
            )
            .unwrap_err(),
            ChoiceError::NoPresentedQuestion
        );
        assert_eq!(
            slot.commit_presented_text(
                &draft.id(),
                "模型随意说的其他问题",
                turn(10),
                &context,
                now
            ),
            Err(ChoiceError::WrongQuestion)
        );
        assert!(slot.latest.is_none());
    }
    #[test]
    fn replacement_rejects_old_question_without_reordering_new_choices() {
        let mut slot = slot();
        let now = Instant::now();
        let context = context();
        let old = presented(&mut slot, &context, &["w2", "w3"], now);
        let new = presented(&mut slot, &context, &["w3", "w2"], now);
        assert_ne!(old.id(), new.id());
        assert_eq!(
            slot.resolve(
                &old.id(),
                turn(11),
                SelectionInput::OwnedText("2"),
                &context,
                now
            )
            .unwrap_err(),
            ChoiceError::WrongQuestion
        );
        assert_eq!(
            slot.resolve(
                &new.id(),
                turn(11),
                SelectionInput::OwnedText("1"),
                &context,
                now
            )
            .unwrap()
            .window_ref(),
            "w3"
        );
    }
    #[test]
    fn closed_or_reused_instance_invalidates_the_whole_ordered_question() {
        let now = Instant::now();
        for mode in 0..3 {
            let mut slot = slot();
            let mut context = context();
            let draft = presented(&mut slot, &context, &["w2", "w3"], now);
            match mode {
                0 => {
                    context["windows"].as_array_mut().unwrap().remove(1);
                }
                1 => context["windows"][1]["target_token"] = json!("reused-new-instance"),
                _ => context["windows"][1]["closed"] = json!(true),
            }
            assert_eq!(
                slot.resolve(
                    &draft.id(),
                    turn(11),
                    SelectionInput::OwnedText("2"),
                    &context,
                    now
                )
                .unwrap_err(),
                ChoiceError::ContextChanged
            );
        }
    }
    #[test]
    fn wm_epoch_next_turn_and_ttl_are_required_even_if_the_ref_still_exists() {
        let now = Instant::now();
        for mode in 0..5 {
            let mut slot = slot();
            let mut context = context();
            let draft = presented(&mut slot, &context, &["w2", "w3"], now);
            let mut answer = turn(11);
            let mut at = now;
            let expected = match mode {
                0 => {
                    context["wm_instance_id"] = json!("wm-2");
                    ChoiceError::ContextChanged
                }
                1 => {
                    answer.session_epoch += 1;
                    ChoiceError::WrongTurn
                }
                2 => {
                    answer.generation += 1;
                    ChoiceError::WrongTurn
                }
                3 => {
                    answer.turn_id += 1;
                    ChoiceError::WrongTurn
                }
                _ => {
                    at += Duration::from_secs(10);
                    ChoiceError::Expired
                }
            };
            assert_eq!(
                slot.resolve(
                    &draft.id(),
                    answer,
                    SelectionInput::OwnedText("first"),
                    &context,
                    at
                )
                .unwrap_err(),
                expected
            );
        }
    }
    #[test]
    fn unrelated_text_negation_new_actions_ambiguity_and_audio_quotes_are_not_selectors() {
        let now = Instant::now();
        let context = context();
        for text in [
            "不是第二个",
            "不要第一个",
            "第二个并关闭它",
            "first then delete",
            "刚才那个",
            "都不是",
            "切到第二个",
            "1; shutdown",
            "",
            "第一个？",
        ] {
            let mut slot = slot();
            let draft = presented(&mut slot, &context, &["w2", "w3"], now);
            assert_eq!(
                slot.resolve(
                    &draft.id(),
                    turn(11),
                    SelectionInput::OwnedText(text),
                    &context,
                    now
                )
                .unwrap_err(),
                ChoiceError::NotPureSelection
            );
            assert!(slot.latest.is_none());
        }
        let mut slot = slot();
        let draft = presented(&mut slot, &context, &["w2", "w3"], now);
        assert_eq!(
            slot.resolve(
                &draft.id(),
                turn(11),
                SelectionInput::UnverifiedAudioQuote("第二个"),
                &context,
                now
            )
            .unwrap_err(),
            ChoiceError::UnverifiedAudioQuote
        );
    }
    #[test]
    fn clear_new_turn_and_out_of_range_do_not_fall_back_to_another_choice() {
        let now = Instant::now();
        let context = context();
        let mut first = slot();
        let draft = presented(&mut first, &context, &["w2", "w3"], now);
        assert_eq!(
            first
                .resolve(
                    &draft.id(),
                    turn(11),
                    SelectionInput::OwnedText("3"),
                    &context,
                    now
                )
                .unwrap_err(),
            ChoiceError::OutOfRange
        );
        for cancel in [true, false] {
            let mut slot = slot();
            let draft = presented(&mut slot, &context, &["w2", "w3"], now);
            if cancel {
                slot.clear();
            } else {
                slot.on_turn(turn(12));
            }
            assert_eq!(
                slot.resolve(
                    &draft.id(),
                    turn(12),
                    SelectionInput::OwnedText("2"),
                    &context,
                    now
                )
                .unwrap_err(),
                ChoiceError::NoPresentedQuestion
            );
        }
    }
    #[test]
    fn indistinguishable_unknown_or_truncated_labels_require_names() {
        let now = Instant::now();
        for mode in 0..3 {
            let mut slot = slot();
            let mut context = context();
            match mode {
                0 => {
                    context["windows"][2] = json!({"window_ref":"w3","target_token":"third","class":"browser","title":"  Documents  "})
                }
                1 => {
                    context["windows"][1] = json!({"window_ref":"w2","target_token":"second","class":"unknown","title":"Untitled"})
                }
                _ => {
                    context["windows"][1]["title"] = json!("A".repeat(100));
                    context["windows"][2]["title"] = json!(format!("{}B", "A".repeat(100)));
                }
            }
            assert_eq!(
                slot.prepare_focus(focus_intent(), &context, &["w2", "w3"], turn(10), now)
                    .unwrap_err(),
                ChoiceError::NeedWindowNames
            );
        }
    }
    #[test]
    fn context_reordering_never_changes_the_presented_ordinal() {
        let mut slot = slot();
        let now = Instant::now();
        let mut context = context();
        let draft = presented(&mut slot, &context, &["w3", "w1", "w2"], now);
        context["windows"].as_array_mut().unwrap().reverse();
        assert_eq!(
            slot.resolve(
                &draft.id(),
                turn(11),
                SelectionInput::OwnedText("third"),
                &context,
                now
            )
            .unwrap()
            .window_ref(),
            "w2"
        );
    }
    #[test]
    fn proof_rechecks_liveness_turn_and_expiry_and_never_contains_an_action() {
        let mut slot = slot();
        let now = Instant::now();
        let mut context = context();
        let draft = presented(&mut slot, &context, &["w2", "w3"], now);
        let selection = slot
            .resolve(
                &draft.id(),
                turn(11),
                SelectionInput::OwnedText(" second "),
                &context,
                now,
            )
            .unwrap();
        assert!(selection.proof().matches_focus_window(
            "focus_window",
            "w3",
            &focus_intent(),
            turn(11),
            &context,
            now
        ));
        assert!(!selection.proof().matches_focus_window(
            "focus_window",
            "w3",
            &focus_intent(),
            turn(12),
            &context,
            now
        ));
        assert!(!selection.proof().matches_focus_window(
            "focus_window",
            "w3",
            &focus_intent(),
            turn(11),
            &context,
            now + Duration::from_secs(10)
        ));
        context["windows"][2]["target_token"] = json!("new token");
        assert!(!selection.proof().matches_focus_window(
            "focus_window",
            "w3",
            &focus_intent(),
            turn(11),
            &context,
            now
        ));
    }
    #[test]
    fn returned_proof_is_revoked_by_replacement_cancellation_unrelated_turn_or_slot_drop() {
        let now = Instant::now();
        let context = context();
        for mode in 0..4 {
            let mut slot = slot();
            let draft = presented(&mut slot, &context, &["w2", "w3"], now);
            let selected = slot
                .resolve(
                    &draft.id(),
                    turn(11),
                    SelectionInput::OwnedText("1"),
                    &context,
                    now,
                )
                .unwrap();
            assert!(selected.proof().matches_focus_window(
                "focus_window",
                "w2",
                &focus_intent(),
                turn(11),
                &context,
                now
            ));
            match mode {
                0 => slot.clear(),
                1 => {
                    slot.prepare_focus(focus_intent(), &context, &["w3", "w2"], turn(11), now)
                        .unwrap();
                }
                2 => slot.on_turn(turn(12)),
                _ => drop(slot),
            }
            assert!(!selected.proof().matches_focus_window(
                "focus_window",
                "w2",
                &focus_intent(),
                turn(11),
                &context,
                now
            ));
        }
    }
    #[test]
    fn sensitive_labels_and_duplicate_native_identity_are_never_offered() {
        let now = Instant::now();
        let mut slot = slot();
        let mut context = context();
        context["windows"][1]["title"] = json!("native-secret-2");
        assert_eq!(
            slot.prepare_focus(focus_intent(), &context, &["w2", "w3"], turn(10), now)
                .unwrap_err(),
            ChoiceError::NeedWindowNames
        );
        context["windows"][1]["title"] = json!("Documents");
        context["windows"].as_array_mut().unwrap().push(json!({"window_ref":"w4","target_token":"native-secret-2","class":"Other","title":"Alias"}));
        assert_eq!(
            slot.prepare_focus(focus_intent(), &context, &["w2", "w3"], turn(10), now)
                .unwrap_err(),
            ChoiceError::InvalidContext
        );
    }
    fn audio_scope(response: &str) -> AudioClaimScope<'_> {
        AudioClaimScope {
            turn: turn(11),
            owned_audio_item_id: "owned-audio-item",
            review_response_id: response,
        }
    }
    fn audio_claim(
        slot: &mut WindowChoiceSlot,
        context: &Value,
        now: Instant,
    ) -> ModelAudioSelectionClaim {
        let draft = presented(slot, context, &["w2", "w3"], now);
        slot.bind_audio_review(&draft.id(), audio_scope("fresh-review"), context, now)
            .unwrap();
        let quote = ModelAudioQuote::from_actor_response(
            &draft.id(),
            audio_scope("fresh-review"),
            "第二个",
            now,
        )
        .unwrap();
        slot.resolve_model_audio_claim(quote, context, now).unwrap()
    }
    #[test]
    fn focus_purpose_requires_actor_affirmed_original_intent_and_never_authorizes_close() {
        for (action, verdict) in [
            ("killclient", ActorIntentVerdict::AffirmativeFocus),
            ("killclient", ActorIntentVerdict::NotFocusOrNegated),
            ("focus_window", ActorIntentVerdict::NotFocusOrNegated),
        ] {
            assert_eq!(
                ActorCheckedFocusIntent::new(action, verdict, turn(10), "never close my browser")
                    .unwrap_err(),
                ChoiceError::PurposeMismatch
            );
        }
        let now = Instant::now();
        let context = context();
        let mut slot = slot();
        let draft = presented(&mut slot, &context, &["w2", "w3"], now);
        assert!(draft.text().starts_with("要切换/聚焦哪个窗口？"));
        assert_eq!(
            serde_json::to_value(&draft).unwrap()["purpose"],
            "focus_window"
        );
        let selected = slot
            .resolve(
                &draft.id(),
                turn(11),
                SelectionInput::OwnedText("1"),
                &context,
                now,
            )
            .unwrap();
        assert!(!selected.proof().matches_focus_window(
            "killclient",
            "w2",
            &focus_intent(),
            turn(11),
            &context,
            now
        ));
        let other_intent = ActorCheckedFocusIntent::new(
            "focus_window",
            ActorIntentVerdict::AffirmativeFocus,
            turn(10),
            "different-original-request",
        )
        .unwrap();
        assert!(!selected.proof().matches_focus_window(
            "focus_window",
            "w2",
            &other_intent,
            turn(11),
            &context,
            now
        ));
    }
    #[test]
    fn audio_claim_is_explicitly_lower_assurance_and_focus_only_not_a_text_proof() {
        let now = Instant::now();
        let context = context();
        let mut slot = slot();
        let claim = audio_claim(&mut slot, &context, now);
        assert_eq!(claim.window_ref(), "w3");
        assert_eq!(claim.assurance(), "unverified_audio");
        assert!(claim.matches_focus_window(
            "focus_window",
            "w3",
            &focus_intent(),
            audio_scope("fresh-review"),
            &context,
            now
        ));
        assert!(!claim.matches_focus_window(
            "killclient",
            "w3",
            &focus_intent(),
            audio_scope("fresh-review"),
            &context,
            now
        ));
        let view = serde_json::to_value(claim.public_claim_view()).unwrap();
        assert_eq!(view["assurance"], "unverified_audio");
        assert_eq!(view["purpose"], "focus_window");
        for key in ["action_approval", "acoustic_proof", "presented_audio_proof"] {
            assert_eq!(view[key], false);
        }
        assert!(
            !view.to_string().contains("native-secret")
                && !format!("{claim:?}").contains("native-secret")
        );
        assert!(format!("{claim:?}").contains("unverified_audio"));
        // The original text API still rejects a model quote, without casting.
        let draft = presented(&mut slot, &context, &["w2", "w3"], now);
        assert_eq!(
            slot.resolve(
                &draft.id(),
                turn(11),
                SelectionInput::UnverifiedAudioQuote("second"),
                &context,
                now
            )
            .unwrap_err(),
            ChoiceError::UnverifiedAudioQuote
        );
    }
    #[test]
    fn audio_metadata_wrong_turn_item_response_and_old_completion_are_rejected() {
        let now = Instant::now();
        let context = context();
        for mode in 0..5 {
            let mut slot = slot();
            let draft = presented(&mut slot, &context, &["w2", "w3"], now);
            slot.bind_audio_review(
                &draft.id(),
                audio_scope("fresh-review"),
                &context,
                now + Duration::from_millis(2),
            )
            .unwrap();
            let mut scope = audio_scope("fresh-review");
            let mut completed = now + Duration::from_millis(3);
            match mode {
                0 => scope.turn = turn(10),
                1 => scope.owned_audio_item_id = "old-audio-item",
                2 => scope.review_response_id = "old-response",
                3 => completed = now,
                _ => completed = now + Duration::from_secs(1),
            }
            let quote =
                ModelAudioQuote::from_actor_response(&draft.id(), scope, "1", completed).unwrap();
            assert_eq!(
                slot.resolve_model_audio_claim(quote, &context, now + Duration::from_millis(4))
                    .unwrap_err(),
                ChoiceError::AudioCorrelationMismatch
            );
        }
    }
    #[test]
    fn replaced_question_or_fresh_response_cannot_consume_an_old_quote() {
        let now = Instant::now();
        let context = context();
        let mut slot = slot();
        let old = presented(&mut slot, &context, &["w2", "w3"], now);
        slot.bind_audio_review(&old.id(), audio_scope("old-response"), &context, now)
            .unwrap();
        let quote =
            ModelAudioQuote::from_actor_response(&old.id(), audio_scope("old-response"), "1", now)
                .unwrap();
        let new = presented(&mut slot, &context, &["w3", "w2"], now);
        slot.bind_audio_review(&new.id(), audio_scope("fresh-review"), &context, now)
            .unwrap();
        assert_eq!(
            slot.resolve_model_audio_claim(quote, &context, now)
                .unwrap_err(),
            ChoiceError::WrongQuestion
        );
        let old_response =
            ModelAudioQuote::from_actor_response(&new.id(), audio_scope("old-response"), "1", now)
                .unwrap();
        assert_eq!(
            slot.resolve_model_audio_claim(old_response, &context, now)
                .unwrap_err(),
            ChoiceError::AudioCorrelationMismatch
        );
        let fresh =
            ModelAudioQuote::from_actor_response(&new.id(), audio_scope("fresh-review"), "1", now)
                .unwrap();
        assert_eq!(
            slot.resolve_model_audio_claim(fresh, &context, now)
                .unwrap()
                .window_ref(),
            "w3"
        );
    }
    #[test]
    fn audio_claims_and_pending_quotes_share_revocation_token_and_ttl_boundaries() {
        let now = Instant::now();
        for mode in 0..6 {
            let mut slot = slot();
            let mut context = context();
            let claim = audio_claim(&mut slot, &context, now);
            match mode {
                0 => slot.clear(),
                1 => {
                    slot.prepare_focus(focus_intent(), &context, &["w3", "w2"], turn(11), now)
                        .unwrap();
                }
                2 => slot.on_turn(turn(12)),
                3 => context["windows"][2]["target_token"] = json!("replacement"),
                4 => context["wm_instance_id"] = json!("new-wm"),
                _ => drop(slot),
            }
            assert!(!claim.matches_focus_window(
                "focus_window",
                "w3",
                &focus_intent(),
                audio_scope("fresh-review"),
                &context,
                now
            ));
        }
        let mut slot = slot();
        let context = context();
        let claim = audio_claim(&mut slot, &context, now);
        assert!(!claim.matches_focus_window(
            "focus_window",
            "w3",
            &focus_intent(),
            audio_scope("fresh-review"),
            &context,
            now + Duration::from_secs(10)
        ));
        let draft = presented(&mut slot, &context, &["w2", "w3"], now);
        slot.bind_audio_review(&draft.id(), audio_scope("fresh-review"), &context, now)
            .unwrap();
        let quote = ModelAudioQuote::from_actor_response(
            &draft.id(),
            audio_scope("fresh-review"),
            "1",
            now,
        )
        .unwrap();
        assert_eq!(
            slot.resolve_model_audio_claim(quote, &context, now + Duration::from_secs(10))
                .unwrap_err(),
            ChoiceError::Expired
        );
    }
    #[test]
    fn audio_quote_needs_registration_and_never_accepts_negation_or_a_new_action() {
        let now = Instant::now();
        let context = context();
        for text in ["不是第二个", "second and close it", "刚才那个", "都不是"] {
            let mut slot = slot();
            let draft = presented(&mut slot, &context, &["w2", "w3"], now);
            let quote = ModelAudioQuote::from_actor_response(
                &draft.id(),
                audio_scope("fresh-review"),
                text,
                now,
            )
            .unwrap();
            assert_eq!(
                slot.resolve_model_audio_claim(quote, &context, now)
                    .unwrap_err(),
                ChoiceError::AudioCorrelationMismatch
            );
            slot.bind_audio_review(&draft.id(), audio_scope("fresh-review"), &context, now)
                .unwrap();
            let quote = ModelAudioQuote::from_actor_response(
                &draft.id(),
                audio_scope("fresh-review"),
                text,
                now,
            )
            .unwrap();
            assert_eq!(
                slot.resolve_model_audio_claim(quote, &context, now)
                    .unwrap_err(),
                ChoiceError::NotPureSelection
            );
        }
    }
}
