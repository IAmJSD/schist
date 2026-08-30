//! Annotation notes: placing, editing and deleting them.

use super::*;

impl Workspace {
    // ----- notes -----

    /// Mirror the persisted note defaults into the shared editor state,
    /// which is where the Note tool reads them from.
    pub(super) fn sync_note_defaults(&mut self) {
        self.editor.note_author = self.view.note_author.clone();
        let c = self.view.note_color;
        self.editor.note_color = Rgba::new(
            ((c >> 16) & 0xFF) as f32 / 255.0,
            ((c >> 8) & 0xFF) as f32 / 255.0,
            (c & 0xFF) as f32 / 255.0,
            1.0,
        );
    }

    /// The note the Notes panel is showing and the canvas is outlining.
    ///
    /// Notes are addressed by index, and undo, delete, Clear Notes and
    /// switching to a document with fewer of them all leave a stale one
    /// behind -- so this resolves rather than trusting what was stored,
    /// and it is the only thing that does. Falls back to the first note
    /// because the panel always shows one, and the marker it outlines has
    /// to be the same one.
    pub fn active_note(&self) -> Option<usize> {
        let n = self.doc.as_ref()?.notes.len();
        (n > 0).then(|| self.editor.active_note.filter(|&i| i < n).unwrap_or(0))
    }

    pub fn toggle_notes(&mut self, cx: &mut Context<Self>) {
        self.view.notes = !self.view.notes;
        self.status = format!("Notes {}", if self.view.notes { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    /// Show a note in the Notes panel. Ends any edit in progress first, so
    /// switching notes mid-typing keeps what was typed into the old one.
    pub fn select_note(&mut self, index: usize, cx: &mut Context<Self>) {
        self.commit_note_edit(cx);
        self.editor.active_note = Some(index);
        cx.notify();
    }

    /// Step to the next (`+1`) or previous (`-1`) note, wrapping.
    pub fn step_note(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.doc.as_ref().map_or(0, |d| d.notes.len());
        if count == 0 {
            return;
        }
        let current = self.active_note().unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(count as isize) as usize;
        self.select_note(next, cx);
    }

    pub fn delete_note(&mut self, index: usize, cx: &mut Context<Self>) {
        self.cancel_note_edit(cx);
        if let Some(doc) = self.doc.as_mut() {
            if index >= doc.notes.len() {
                return;
            }
            let mut edit = doc.begin_edit("Delete Note");
            edit.change_notes(|notes| {
                notes.remove(index);
            });
            edit.commit();
        }
        // Keep reading down the list, as Photoshop's Notes panel does,
        // rather than dropping the selection on every delete.
        let remaining = self.doc.as_ref().map_or(0, |d| d.notes.len());
        self.editor.active_note = (remaining > 0).then(|| index.min(remaining - 1));
        self.after_change(cx);
    }

    pub fn clear_notes(&mut self, cx: &mut Context<Self>) {
        self.cancel_note_edit(cx);
        if let Some(doc) = self.doc.as_mut() {
            if doc.notes.is_empty() {
                return;
            }
            let mut edit = doc.begin_edit("Clear Notes");
            edit.change_notes(|notes| notes.clear());
            edit.commit();
        }
        self.editor.active_note = None;
        self.after_change(cx);
    }

    /// Set the author stamped on notes placed from now on. Existing notes
    /// keep the name they were written under.
    pub fn set_note_author(&mut self, author: String, cx: &mut Context<Self>) {
        self.view.note_author = author;
        self.sync_note_defaults();
        self.save_view_options();
        cx.notify();
    }

    /// Start typing into a note's body in the Notes panel.
    pub fn begin_note_edit(&mut self, index: usize, cx: &mut Context<Self>) {
        self.commit_note_edit(cx);
        let Some(text) = self
            .doc
            .as_ref()
            .and_then(|d| d.notes.get(index))
            .map(|n| n.text.clone())
        else {
            return;
        };
        self.editor.active_note = Some(index);
        self.note_edit = Some((NoteField::Text(index), text));
        cx.notify();
    }

    /// Start typing into the Author field on the options bar.
    pub fn begin_note_author_edit(&mut self, cx: &mut Context<Self>) {
        self.commit_note_edit(cx);
        self.note_edit = Some((NoteField::Author, self.view.note_author.clone()));
        cx.notify();
    }

    /// What an open field is showing, for the renderer to draw a caret
    /// after. `None` when that field is not the one being typed into.
    pub fn note_edit_buffer(&self, field: NoteField) -> Option<&str> {
        match &self.note_edit {
            Some((f, text)) if *f == field => Some(text),
            _ => None,
        }
    }

    /// Write the typed text back as one history entry for the session.
    ///
    /// Unlike a layer rename an empty note is kept: a note with no text is
    /// a legitimate pin, and clearing one should not silently restore what
    /// it used to say.
    pub fn commit_note_edit(&mut self, cx: &mut Context<Self>) {
        let Some((field, text)) = self.note_edit.take() else {
            return;
        };
        match field {
            NoteField::Text(index) => {
                if let Some(doc) = self.doc.as_mut() {
                    if doc.notes.get(index).is_some_and(|n| n.text != text) {
                        let mut edit = doc.begin_edit("Edit Note");
                        edit.change_notes(|notes| notes[index].text = text);
                        edit.commit();
                    }
                }
            }
            // Not a document edit, so it is not in the history: the author
            // is a preference, and undoing a brush stroke should not
            // rename the person who made it.
            NoteField::Author => self.set_note_author(text, cx),
        }
        self.after_change(cx);
    }

    /// Abandon the typing session, leaving the note as it was.
    pub fn cancel_note_edit(&mut self, cx: &mut Context<Self>) {
        if self.note_edit.take().is_some() {
            cx.notify();
        }
    }

    /// Feed a keystroke to an open note. Consumes every key while one is
    /// open, so single-letter tool shortcuts can't fire mid-sentence.
    ///
    /// Enter inserts a newline rather than committing: a note is a
    /// paragraph, not a field, and Photoshop's is multi-line too. Escape
    /// and clicking away are what end the session.
    pub fn note_edit_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.note_edit.is_none() {
            return false;
        }
        match ev.keystroke.key.as_str() {
            "escape" | "tab" => self.commit_note_edit(cx),
            // A note's body is a paragraph, so Enter breaks the line, as
            // it does in Photoshop's Notes panel. The one-line Author
            // field has nothing to break, so there Enter means done.
            "enter"
                if self
                    .note_edit
                    .as_ref()
                    .is_some_and(|(f, _)| *f == NoteField::Author) =>
            {
                self.commit_note_edit(cx)
            }
            "enter" => {
                if let Some((_, text)) = self.note_edit.as_mut() {
                    text.push('\n');
                }
            }
            "backspace" => {
                if let Some((_, text)) = self.note_edit.as_mut() {
                    text.pop();
                }
            }
            "space" => {
                if let Some((_, text)) = self.note_edit.as_mut() {
                    text.push(' ');
                }
            }
            _ => {
                if let (Some((_, text)), Some(t)) =
                    (self.note_edit.as_mut(), ev.keystroke.key_char.as_deref())
                {
                    if !t.is_empty() && !t.chars().any(char::is_control) {
                        text.push_str(t);
                    }
                }
            }
        }
        cx.notify();
        true
    }
}
