use crate::style::InlineStyle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockTextSize {
    Small,
    Normal,
    Large,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Heading { level: u8 },
    UnorderedListItem,
    OrderedListItem,
    BlockQuote,
    HorizontalRule,
    CodeBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockFormat {
    pub kind: BlockKind,
    pub size: BlockTextSize,
}

impl Default for BlockFormat {
    fn default() -> Self {
        Self {
            kind: BlockKind::Paragraph,
            size: BlockTextSize::Normal,
        }
    }
}

impl BlockFormat {
    pub(crate) fn split_successor(self) -> Self {
        match self.kind {
            BlockKind::Heading { .. } | BlockKind::HorizontalRule => BlockFormat::default(),
            BlockKind::Paragraph
            | BlockKind::UnorderedListItem
            | BlockKind::OrderedListItem
            | BlockKind::BlockQuote
            | BlockKind::CodeBlock => self,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RichTextDocument {
    pub blocks: Vec<BlockNode>,
}

impl Default for RichTextDocument {
    fn default() -> Self {
        Self {
            blocks: vec![BlockNode::default()],
        }
    }
}

impl RichTextDocument {
    pub fn from_plain_text(text: &str) -> Self {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut blocks: Vec<BlockNode> = text
            .split('\n')
            .map(|line| BlockNode::from_text(BlockFormat::default(), line))
            .collect();

        if blocks.is_empty() {
            blocks.push(BlockNode::default());
        }

        Self { blocks }
    }

    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        for (ix, block) in self.blocks.iter().enumerate() {
            if ix > 0 {
                out.push('\n');
            }
            out.push_str(&block.to_plain_text());
        }
        out
    }

    /// Serialize the document back to markdown source so the LLM receives the
    /// structure the user authored (headings, lists, code fences, links, …)
    /// rather than plain text with markers stripped.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let mut ordered = 1usize;
        let mut i = 0;
        while i < self.blocks.len() {
            let block = &self.blocks[i];
            match block.format.kind {
                BlockKind::HorizontalRule => {
                    ordered = 1;
                    out.push_str("---\n");
                    i += 1;
                }
                BlockKind::CodeBlock => {
                    ordered = 1;
                    out.push_str("```\n");
                    while i < self.blocks.len()
                        && matches!(self.blocks[i].format.kind, BlockKind::CodeBlock)
                    {
                        out.push_str(&inlines_to_markdown(&self.blocks[i].inlines));
                        out.push('\n');
                        i += 1;
                    }
                    out.push_str("```\n");
                }
                BlockKind::Heading { level } => {
                    ordered = 1;
                    out.push_str(&"#".repeat(level.min(6) as usize));
                    out.push(' ');
                    out.push_str(&inlines_to_markdown(&block.inlines));
                    out.push('\n');
                    i += 1;
                }
                BlockKind::UnorderedListItem => {
                    ordered = 1;
                    out.push_str("- ");
                    out.push_str(&inlines_to_markdown(&block.inlines));
                    out.push('\n');
                    i += 1;
                }
                BlockKind::OrderedListItem => {
                    out.push_str(&format!("{}. ", ordered));
                    ordered += 1;
                    out.push_str(&inlines_to_markdown(&block.inlines));
                    out.push('\n');
                    i += 1;
                }
                BlockKind::BlockQuote => {
                    ordered = 1;
                    out.push_str("> ");
                    out.push_str(&inlines_to_markdown(&block.inlines));
                    out.push('\n');
                    i += 1;
                }
                BlockKind::Paragraph => {
                    ordered = 1;
                    out.push_str(&inlines_to_markdown(&block.inlines));
                    out.push('\n');
                    i += 1;
                }
            }
        }
        if out.ends_with('\n') {
            out.pop();
        }
        out
    }
}

fn inlines_to_markdown(inlines: &[InlineNode]) -> String {
    let mut out = String::new();
    for node in inlines {
        match node {
            InlineNode::Text(text) => out.push_str(&inline_to_markdown(text)),
        }
    }
    out
}

fn inline_to_markdown(text: &TextNode) -> String {
    let s = &text.text;
    let st = &text.style;

    if st.code {
        return format!("`{s}`");
    }

    let mut out = String::new();
    if st.link_url.is_some() {
        out.push('[');
    }
    if st.strikethrough {
        out.push_str("~~");
    }
    if st.bold {
        out.push_str("**");
    }
    if st.italic {
        out.push('*');
    }
    out.push_str(s);
    if st.italic {
        out.push('*');
    }
    if st.bold {
        out.push_str("**");
    }
    if st.strikethrough {
        out.push_str("~~");
    }
    if let Some(url) = &st.link_url {
        out.push_str("](");
        out.push_str(url);
        out.push(')');
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockNode {
    pub format: BlockFormat,
    pub inlines: Vec<InlineNode>,
}

impl Default for BlockNode {
    fn default() -> Self {
        Self::from_text(BlockFormat::default(), "")
    }
}

impl BlockNode {
    pub fn from_text(format: BlockFormat, text: &str) -> Self {
        Self {
            format,
            inlines: vec![InlineNode::Text(TextNode {
                text: text.to_string(),
                style: InlineStyle::default(),
            })],
        }
    }

    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        for node in &self.inlines {
            match node {
                InlineNode::Text(text) => out.push_str(&text.text),
            }
        }
        out
    }

    pub fn text_len(&self) -> usize {
        self.inlines.iter().fold(0usize, |acc, node| match node {
            InlineNode::Text(text) => acc + text.text.len(),
        })
    }

    pub fn is_text_empty(&self) -> bool {
        self.text_len() == 0
    }

    pub fn normalize(&mut self) {
        let mut normalized: Vec<InlineNode> = Vec::with_capacity(self.inlines.len());

        for node in self.inlines.drain(..) {
            match node {
                InlineNode::Text(text) => {
                    if let Some(InlineNode::Text(prev)) = normalized.last_mut() {
                        if prev.style == text.style {
                            prev.text.push_str(&text.text);
                            continue;
                        }
                    }
                    normalized.push(InlineNode::Text(text));
                }
            }
        }

        let has_any_text = normalized.iter().any(|node| match node {
            InlineNode::Text(text) => !text.text.is_empty(),
        });

        if has_any_text {
            normalized.retain(|node| match node {
                InlineNode::Text(text) => !text.text.is_empty(),
            });
        }

        if normalized.is_empty() {
            normalized.push(InlineNode::Text(TextNode {
                text: String::new(),
                style: InlineStyle::default(),
            }));
        }

        self.inlines = normalized;
    }

    pub fn last_style(&self) -> Option<&InlineStyle> {
        self.inlines.iter().rev().find_map(|node| match node {
            InlineNode::Text(text) => (!text.text.is_empty()).then_some(&text.style),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InlineNode {
    Text(TextNode),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextNode {
    pub text: String,
    pub style: InlineStyle,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(text: &str) -> BlockNode {
        BlockNode::from_text(BlockFormat::default(), text)
    }

    fn styled(text: &str, style: InlineStyle) -> BlockNode {
        BlockNode {
            format: BlockFormat::default(),
            inlines: vec![InlineNode::Text(TextNode {
                text: text.to_string(),
                style,
            })],
        }
    }

    #[test]
    fn to_markdown_paragraphs() {
        let doc = RichTextDocument {
            blocks: vec![para("hello"), para("world")],
        };
        assert_eq!(doc.to_markdown(), "hello\nworld");
    }

    #[test]
    fn to_markdown_heading_and_quote() {
        let doc = RichTextDocument {
            blocks: vec![
                BlockNode::from_text(
                    BlockFormat {
                        kind: BlockKind::Heading { level: 2 },
                        size: BlockTextSize::Normal,
                    },
                    "Title",
                ),
                BlockNode::from_text(
                    BlockFormat {
                        kind: BlockKind::BlockQuote,
                        size: BlockTextSize::Normal,
                    },
                    "quoted",
                ),
            ],
        };
        assert_eq!(doc.to_markdown(), "## Title\n> quoted");
    }

    #[test]
    fn to_markdown_lists() {
        let doc = RichTextDocument {
            blocks: vec![
                BlockNode::from_text(
                    BlockFormat {
                        kind: BlockKind::UnorderedListItem,
                        size: BlockTextSize::Normal,
                    },
                    "a",
                ),
                BlockNode::from_text(
                    BlockFormat {
                        kind: BlockKind::OrderedListItem,
                        size: BlockTextSize::Normal,
                    },
                    "b",
                ),
                BlockNode::from_text(
                    BlockFormat {
                        kind: BlockKind::OrderedListItem,
                        size: BlockTextSize::Normal,
                    },
                    "c",
                ),
            ],
        };
        assert_eq!(doc.to_markdown(), "- a\n1. b\n2. c");
    }

    #[test]
    fn to_markdown_horizontal_rule() {
        let doc = RichTextDocument {
            blocks: vec![para("above"), para("---"), para("below")],
        };
        // Plain paragraph with "---" stays literal.
        assert_eq!(doc.to_markdown(), "above\n---\nbelow");
    }

    #[test]
    fn to_markdown_code_block_run() {
        let code = BlockFormat {
            kind: BlockKind::CodeBlock,
            size: BlockTextSize::Normal,
        };
        let doc = RichTextDocument {
            blocks: vec![
                para("intro"),
                BlockNode::from_text(code, "let x = 1;"),
                BlockNode::from_text(code, "let y = 2;"),
                para("tail"),
            ],
        };
        assert_eq!(
            doc.to_markdown(),
            "intro\n```\nlet x = 1;\nlet y = 2;\n```\ntail"
        );
    }

    #[test]
    fn to_markdown_inline_marks_and_link() {
        let doc = RichTextDocument {
            blocks: vec![BlockNode {
                format: BlockFormat::default(),
                inlines: vec![
                    InlineNode::Text(TextNode {
                        text: "b".to_string(),
                        style: InlineStyle {
                            bold: true,
                            ..Default::default()
                        },
                    }),
                    InlineNode::Text(TextNode {
                        text: "i".to_string(),
                        style: InlineStyle {
                            italic: true,
                            ..Default::default()
                        },
                    }),
                    InlineNode::Text(TextNode {
                        text: "c".to_string(),
                        style: InlineStyle {
                            code: true,
                            ..Default::default()
                        },
                    }),
                    InlineNode::Text(TextNode {
                        text: "click".to_string(),
                        style: InlineStyle {
                            link_url: Some("https://example.com".to_string()),
                            ..Default::default()
                        },
                    }),
                ],
            }],
        };
        assert_eq!(doc.to_markdown(), "**b***i*`c`[click](https://example.com)");
    }

    #[test]
    fn to_markdown_image_placeholder_literal() {
        // Image placeholders are plain text runs; they serialize verbatim.
        let doc = RichTextDocument {
            blocks: vec![styled("[Image1]", InlineStyle::default())],
        };
        assert_eq!(doc.to_markdown(), "[Image1]");
    }
}
