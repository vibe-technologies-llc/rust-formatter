mod attributes;
mod paragraphs;
mod skip;
mod text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pass {
    pub sort_attributes: bool,
    pub max_blank_lines: Option<usize>,
}

pub fn apply(text: &str, pass: Pass) -> String {
    let sorted = if pass.sort_attributes {
        attributes::apply(text)
    } else {
        text.to_owned()
    };

    match pass.max_blank_lines {
        Some(max_blank_lines) => paragraphs::apply(&sorted, max_blank_lines),
        None => sorted,
    }
}
