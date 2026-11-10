//! Grouped request-user-input form. Selections and notes belong to each question, not history.
use super::*;

pub(super) struct FormQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<(String, String)>,
    draft: Option<composer::Draft>,
    pub selected: usize,
    pub notes: bool,
    pub(super) committed: bool,
}

pub(super) struct QuestionForm {
    pub questions: Vec<FormQuestion>,
    pub index: usize,
    pub confirm: Option<usize>,
}

impl Tui {
    pub(super) fn form_layout(question: &FormQuestion, cols: usize, rows: usize) -> (Vec<String>, Vec<Vec<String>>, usize, usize) {
        let prompt = super::text::wrap_terminal(&question.question, cols).into_iter().take(rows.saturating_sub(6).clamp(1, 3)).collect::<Vec<_>>();
        let editor_rows = rows.saturating_sub(prompt.len() + 5).clamp(1, 3);
        let room = rows.saturating_sub(prompt.len() + editor_rows + 3).max(1);
        let options = question
            .options
            .iter()
            .chain(std::iter::once(&("Other".into(), "Write your own answer in notes".into())))
            .enumerate()
            .map(|(i, (label, description))| super::text::wrap_terminal(&format!("{} {}. {label}  {description}", if i == question.selected { "›" } else { " " }, i + 1), cols))
            .collect();
        (prompt, options, editor_rows, room)
    }

    pub async fn ask_user_questions(&self, questions: Vec<Value>, cancel: &CancellationToken) -> Result<IndexMap<String, Vec<String>>> {
        if questions.is_empty() {
            return Ok(IndexMap::new());
        }
        let questions = questions
            .into_iter()
            .map(|q| FormQuestion {
                id: q.get("id").and_then(Value::as_str).unwrap_or_default().into(),
                header: terminal_text(q.get("header").and_then(Value::as_str).unwrap_or("Question")),
                question: terminal_text(q.get("question").and_then(Value::as_str).unwrap_or_default()),
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|option| {
                        (terminal_text(option.get("label").and_then(Value::as_str).unwrap_or_default()), terminal_text(option.get("description").and_then(Value::as_str).unwrap_or_default()))
                    })
                    .collect(),
                draft: None,
                selected: 0,
                notes: false,
                committed: false,
            })
            .collect();
        let form = QuestionForm { questions, index: 0, confirm: None };
        let encoded = self.question_inner("Answer: ", Some(cancel), false, QuestionOpts { esc_cancels: true, ..Default::default() }, Some(form)).await?;
        Ok(serde_json::from_str(&encoded)?)
    }

    fn save_form_draft(st: &mut State) {
        let draft = composer::Draft::capture(st);
        let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
        form.questions[form.index].draft = Some(draft);
    }

    fn move_form_question(&self, st: &mut State, forward: bool) {
        Self::save_form_draft(st);
        let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
        let next = if forward { (form.index + 1).min(form.questions.len() - 1) } else { form.index.saturating_sub(1) };
        if next == form.index {
            return;
        }
        form.index = next;
        let draft = form.questions[next].draft.clone();
        if let Some(draft) = draft {
            draft.restore(st);
        } else {
            Self::clear_composer(st);
        }
        self.schedule(st, 0);
    }

    fn return_to_unanswered(&self, st: &mut State) {
        Self::save_form_draft(st);
        let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
        form.confirm = None;
        form.index = form.questions.iter().position(|q| !q.committed).unwrap_or(form.index);
        let draft = form.questions[form.index].draft.clone();
        if let Some(draft) = draft {
            draft.restore(st);
        } else {
            Self::clear_composer(st);
        }
    }

    fn submit_form(st: &mut State) {
        let mut answers = IndexMap::new();
        let mut pending = st.pending.take().unwrap();
        for question in pending.form.take().unwrap().questions {
            let mut answer = Vec::new();
            if question.committed {
                if let Some((label, _)) = question.options.get(question.selected) {
                    answer.push(label.clone());
                }
                if let Some(draft) = question.draft {
                    draft.restore(st);
                    let raw = st.input.iter().collect::<String>();
                    let notes = composer::QueuedPrompt::capture(st, &raw).expand();
                    if !notes.trim().is_empty() {
                        answer.push(notes.trim().into());
                    }
                }
            }
            answers.insert(question.id, answer);
        }
        Self::clear_composer(st);
        let _ = pending.tx.send(Ok(serde_json::to_string(&answers).unwrap()));
    }

    pub(super) fn form_key(&self, st: &mut State, text: Option<&str>, key: &Key) -> bool {
        if let Some(selected) = st.pending.as_ref().unwrap().form.as_ref().unwrap().confirm {
            if key.ctrl && key.is("c") {
                let pending = st.pending.take().unwrap();
                let _ = pending.tx.send(Err(QErr::Esc));
            } else if key.is("escape") || key.is("backspace") {
                self.return_to_unanswered(st);
            } else if key.is("return") || key.is("enter") {
                if selected == 0 {
                    Self::submit_form(st);
                } else {
                    self.return_to_unanswered(st);
                }
            } else {
                let selected = if key.is("up") || key.is("down") || key.is("j") || key.is("k") {
                    Some(1 - selected)
                } else {
                    text.and_then(|s| s.parse::<usize>().ok()).filter(|n| *n == 1 || *n == 2).map(|n| n - 1)
                };
                if let Some(selected) = selected {
                    st.pending.as_mut().unwrap().form.as_mut().unwrap().confirm = Some(selected);
                }
            }
            self.schedule(st, 0);
            return true;
        }
        let form = st.pending.as_ref().unwrap().form.as_ref().unwrap();
        let question = &form.questions[form.index];
        let notes = question.notes || question.options.is_empty();
        if key.is("escape") || (key.ctrl && (key.is("c") || key.is("d"))) {
            let pending = st.pending.take().unwrap();
            let _ = pending.tx.send(Err(QErr::Esc));
            self.schedule(st, 0);
            return true;
        }
        if key.is("pageup") || (key.ctrl && key.is("p")) || (!notes && key.is("left")) {
            self.move_form_question(st, false);
            return true;
        }
        if key.is("pagedown") || (key.ctrl && key.is("n")) || (!notes && key.is("right")) {
            self.move_form_question(st, true);
            return true;
        }
        if key.is("tab") {
            let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
            let question = &mut form.questions[form.index];
            question.notes = !question.notes;
            self.schedule(st, 0);
            return true;
        }
        if notes && key.is("backspace") && st.input.is_empty() && !question.options.is_empty() {
            let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
            form.questions[form.index].notes = false;
            self.schedule(st, 0);
            return true;
        }
        if notes && (key.is("up") || key.is("down")) && !question.options.is_empty() {
            let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
            let question = &mut form.questions[form.index];
            let count = question.options.len() + 1;
            question.selected = if key.is("up") { (question.selected + count - 1) % count } else { (question.selected + 1) % count };
            question.committed = false;
            self.schedule(st, 0);
            return true;
        }
        if !notes && (key.is("up") || key.is("down")) {
            let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
            let question = &mut form.questions[form.index];
            let count = question.options.len() + 1; // Other is a real selectable row.
            question.selected = if key.is("up") { (question.selected + count - 1) % count } else { (question.selected + 1) % count };
            question.committed = false;
            self.schedule(st, 0);
            return true;
        }
        let submit = (key.is("return") || key.is("enter")) && !key.shift && !key.ctrl && !key.meta;
        let number = if notes { None } else { text.and_then(|t| t.parse::<usize>().ok()).filter(|n| *n >= 1 && *n <= question.options.len() + 1) };
        if submit || number.is_some() {
            if let Some(number) = number {
                let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
                form.questions[form.index].selected = number - 1;
            }
            let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
            let question = &mut form.questions[form.index];
            if !notes && question.selected == question.options.len() {
                question.notes = true;
                self.schedule(st, 0);
                return true;
            }
            if question.selected < question.options.len() {
                let (columns, rows) = self.size();
                let (_, options, _, room) = Self::form_layout(question, columns.saturating_sub(1).max(3), rows.max(8));
                if options[question.selected].len() > room {
                    st.status_notice = Some("Expand terminal to read the entire option before submitting".into());
                    self.schedule(st, 0);
                    return true;
                }
            }
            question.committed = question.selected < question.options.len() || st.input.iter().any(|c| !c.is_whitespace());
            Self::save_form_draft(st);
            let form = st.pending.as_ref().unwrap().form.as_ref().unwrap();
            if form.index + 1 < form.questions.len() {
                self.move_form_question(st, true);
            } else if form.questions.iter().any(|q| !q.committed) {
                st.pending.as_mut().unwrap().form.as_mut().unwrap().confirm = Some(0);
            } else {
                Self::submit_form(st);
            }
            self.schedule(st, 0);
            return true;
        }
        if !notes && text.is_some() && !key.ctrl && !key.meta {
            let form = st.pending.as_mut().unwrap().form.as_mut().unwrap();
            form.questions[form.index].notes = true;
        }
        // The shared composer handles editing, Unicode motion, kill/yank and multiline notes.
        false
    }
}
