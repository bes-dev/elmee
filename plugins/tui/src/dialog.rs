//! Interactive questions in the terminal frontend.

use elmee_core::{InteractionRequest, InteractionResponse, interaction::Question};

use super::input::Editor;

pub struct Dialog {
    pub request: InteractionRequest,
    pub origin: Option<String>,
    pub question: usize,
    pub cursor: usize,
    pub marked: Vec<bool>,
    pub answers: Vec<Vec<String>>,
    pub editor: Editor,
    pub typing: bool,
}

impl Dialog {
    #[cfg(test)]
    pub fn new(request: InteractionRequest) -> Self {
        Self::with_origin(request, None)
    }

    pub fn with_origin(request: InteractionRequest, origin: Option<String>) -> Self {
        let InteractionRequest::Questions { questions, .. } = &request;
        let count = questions[0].options.len() + 1;
        Self {
            request,
            origin,
            question: 0,
            cursor: 0,
            marked: vec![false; count],
            answers: Vec::new(),
            editor: Editor::default(),
            typing: false,
        }
    }

    pub fn id(&self) -> &str {
        match &self.request {
            InteractionRequest::Questions { id, .. } => id,
        }
    }

    pub fn current_question(&self) -> Option<&Question> {
        match &self.request {
            InteractionRequest::Questions { questions, .. } => questions.get(self.question),
        }
    }

    pub fn option_count(&self) -> usize {
        self.current_question().map_or(4, |question| question.options.len() + 1)
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.option_count() as isize;
        self.cursor = ((self.cursor as isize + delta + len) % len) as usize;
    }

    pub fn toggle(&mut self) {
        if self.current_question().is_some_and(|question| question.multi_select)
            && self.cursor < self.marked.len() - 1
        {
            self.marked[self.cursor] = !self.marked[self.cursor];
        }
    }

    pub fn submit(&mut self) -> Option<InteractionResponse> {
        if self.typing {
            let text = self.editor.take();
            return self.answer(vec![text]);
        }
        match &self.request {
            InteractionRequest::Questions { questions, .. } => {
                let question = &questions[self.question];
                if self.cursor == question.options.len() {
                    self.typing = true;
                    return None;
                }
                let values = if question.multi_select {
                    if !self.marked.iter().any(|marked| *marked) {
                        self.marked[self.cursor] = true;
                    }
                    question
                        .options
                        .iter()
                        .zip(&self.marked)
                        .filter(|(_, marked)| **marked)
                        .map(|(option, _)| option.label.clone())
                        .collect()
                } else {
                    vec![question.options[self.cursor].label.clone()]
                };
                self.answer(values)
            }
        }
    }

    fn answer(&mut self, values: Vec<String>) -> Option<InteractionResponse> {
        self.answers.push(values);
        let InteractionRequest::Questions { questions, .. } = &self.request;
        let total = questions.len();
        if self.answers.len() == total {
            return Some(InteractionResponse::Answers(std::mem::take(&mut self.answers)));
        }
        self.question += 1;
        self.cursor = 0;
        self.typing = false;
        self.editor.clear();
        self.marked = vec![false; self.option_count()];
        None
    }

    pub fn cancel(&self) -> InteractionResponse {
        InteractionResponse::Cancel
    }
}

#[cfg(test)]
mod tests {
    use elmee_core::interaction::OptionItem;

    use super::*;

    fn questions(multi_select: bool) -> InteractionRequest {
        InteractionRequest::Questions {
            id: "1".into(),
            questions: vec![Question {
                header: "Scope".into(),
                question: "What should change?".into(),
                options: vec![
                    OptionItem { label: "Core".into(), description: "Engine".into() },
                    OptionItem { label: "TUI".into(), description: "Interface".into() },
                ],
                multi_select,
            }],
        }
    }

    #[test]
    fn selects_one_or_many_answers() {
        let mut single = Dialog::new(questions(false));
        single.move_cursor(1);
        assert!(
            matches!(single.submit(), Some(InteractionResponse::Answers(value)) if value == vec![vec!["TUI"]])
        );

        let mut many = Dialog::new(questions(true));
        many.toggle();
        many.move_cursor(1);
        many.toggle();
        assert!(
            matches!(many.submit(), Some(InteractionResponse::Answers(value)) if value == vec![vec!["Core", "TUI"]])
        );
    }

    #[test]
    fn other_collects_free_text() {
        let mut dialog = Dialog::new(questions(false));
        dialog.move_cursor(-1);
        assert!(dialog.submit().is_none() && dialog.typing);
        dialog.editor.insert_str("Both, but core first");
        assert!(
            matches!(dialog.submit(), Some(InteractionResponse::Answers(value)) if value == vec![vec!["Both, but core first"]])
        );
    }
}
