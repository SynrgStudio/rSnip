use super::EditorAnnotation;

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorSnapshot {
    annotations: Vec<EditorAnnotation>,
    next_step_number: u32,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EditHistory {
    undo_stack: Vec<EditorSnapshot>,
}

impl EditHistory {
    pub fn push(&mut self, annotations: &[EditorAnnotation], next_step_number: u32) {
        self.undo_stack.push(EditorSnapshot {
            annotations: annotations.to_vec(),
            next_step_number,
        });
    }

    pub fn undo(&mut self) -> Option<(Vec<EditorAnnotation>, u32)> {
        self.undo_stack
            .pop()
            .map(|snapshot| (snapshot.annotations, snapshot.next_step_number))
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn clear(&mut self) {
        self.undo_stack.clear();
    }
}
