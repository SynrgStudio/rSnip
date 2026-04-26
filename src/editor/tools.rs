#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorTool {
    Pen,
    Arrow,
    Line,
    Rectangle,
    Redact,
    Step,
}

impl EditorTool {
    pub const SHORTCUTS: [(char, Self); 6] = [
        ('1', Self::Pen),
        ('2', Self::Arrow),
        ('3', Self::Line),
        ('4', Self::Rectangle),
        ('5', Self::Redact),
        ('6', Self::Step),
    ];

    pub fn from_shortcut(shortcut: char) -> Option<Self> {
        Self::SHORTCUTS
            .iter()
            .find_map(|(key, tool)| (*key == shortcut).then_some(*tool))
    }
}

impl Default for EditorTool {
    fn default() -> Self {
        Self::Pen
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorColor {
    Red,
    Blue,
    Green,
    Yellow,
}

impl EditorColor {
    pub const SHORTCUTS: [(char, Self); 4] = [
        ('q', Self::Red),
        ('w', Self::Blue),
        ('e', Self::Green),
        ('r', Self::Yellow),
    ];

    pub fn from_shortcut(shortcut: char) -> Option<Self> {
        let shortcut = shortcut.to_ascii_lowercase();
        Self::SHORTCUTS
            .iter()
            .find_map(|(key, color)| (*key == shortcut).then_some(*color))
    }

    pub fn rgba(self) -> [u8; 4] {
        match self {
            Self::Red => [0xff, 0x33, 0x33, 0xff],
            Self::Blue => [0x33, 0x88, 0xff, 0xff],
            Self::Green => [0x33, 0xcc, 0x33, 0xff],
            Self::Yellow => [0xff, 0xcc, 0x00, 0xff],
        }
    }
}

impl Default for EditorColor {
    fn default() -> Self {
        Self::Red
    }
}

#[cfg(test)]
mod tests {
    use super::{EditorColor, EditorTool};

    #[test]
    fn resolves_tool_shortcuts() {
        assert_eq!(EditorTool::from_shortcut('1'), Some(EditorTool::Pen));
        assert_eq!(EditorTool::from_shortcut('6'), Some(EditorTool::Step));
        assert_eq!(EditorTool::from_shortcut('x'), None);
    }

    #[test]
    fn resolves_color_shortcuts() {
        assert_eq!(EditorColor::from_shortcut('q'), Some(EditorColor::Red));
        assert_eq!(EditorColor::from_shortcut('W'), Some(EditorColor::Blue));
        assert_eq!(EditorColor::from_shortcut('x'), None);
    }
}
