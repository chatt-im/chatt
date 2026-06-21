use std::ops::Range;

fn ranges(text: &str, first_width: usize, cont_width: usize) -> Vec<Range<usize>> {
    let out: Vec<_> = bwrap::wrap_ranges(text, first_width, cont_width).collect();
    for range in &out {
        let line = &text[range.clone()];
        assert!(!line.is_empty(), "empty line range {range:?} for {text:?}");
        assert!(
            !line.starts_with([' ', '\t', '\n', '\r']) && !line.ends_with([' ', '\t', '\n', '\r']),
            "line {line:?} has boundary whitespace for {text:?}"
        );
    }
    out
}

fn lines<'a>(text: &'a str, first_width: usize, cont_width: usize) -> Vec<&'a str> {
    ranges(text, first_width, cont_width)
        .into_iter()
        .map(|r| &text[r])
        .collect()
}

#[test]
fn basic_wrap() {
    assert_eq!(
        lines("one two three one two three", 13, 13),
        vec!["one two three", "one two three"]
    );
}

#[test]
fn word_exactly_at_width() {
    assert_eq!(lines("abc de", 3, 3), vec!["abc", "de"]);
}

#[test]
fn hanging_indent_widths() {
    assert_eq!(
        lines("aaa bbb ccc ddd", 10, 6),
        vec!["aaa bbb", "ccc", "ddd"]
    );
}

#[test]
fn soft_newline_joins_into_one_line() {
    let text = "foo\n   bar";
    assert_eq!(ranges(text, 8, 8), vec![0..text.len()]);
}

#[test]
fn newline_run_counts_as_width_one() {
    let text = "foo\n   bar";
    assert_eq!(ranges(text, 7, 7), vec![0..text.len()]);
    assert_eq!(lines(text, 6, 6), vec!["foo", "bar"]);
}

#[test]
fn crlf_is_a_soft_break() {
    let text = "foo\r\nbar";
    assert_eq!(ranges(text, 7, 7), vec![0..text.len()]);
    assert_eq!(lines(text, 4, 4), vec!["foo", "bar"]);
}

#[test]
fn inner_spaces_keep_literal_width() {
    assert_eq!(lines("a  b", 4, 4), vec!["a  b"]);
    assert_eq!(lines("a  b", 3, 3), vec!["a", "b"]);
}

#[test]
fn long_ascii_word_chunked() {
    assert_eq!(lines("abcdefghij", 4, 4), vec!["abcd", "efgh", "ij"]);
}

#[test]
fn long_word_uses_cont_width_after_first_chunk() {
    assert_eq!(lines("abcdefghij", 3, 5), vec!["abc", "defgh", "ij"]);
}

#[test]
fn cjk_double_width_chunked() {
    assert_eq!(lines("你好世界", 4, 4), vec!["你好", "世界"]);
    assert_eq!(lines("你好世界", 3, 3), vec!["你", "好", "世", "界"]);
}

#[test]
fn zwj_emoji_cluster_never_split() {
    let text = "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    assert_eq!(lines(text, 1, 1), vec![text]);
}

#[test]
fn trailing_whitespace_trimmed() {
    assert_eq!(ranges("hello   ", 10, 10), vec![0..5]);
    assert_eq!(ranges("hello \n ", 10, 10), vec![0..5]);
}

#[test]
fn leading_whitespace_skipped() {
    assert_eq!(ranges("  \n hello", 10, 10), vec![4..9]);
}

#[test]
fn empty_and_blank_yield_nothing() {
    assert_eq!(lines("", 10, 10), Vec::<&str>::new());
    assert_eq!(lines("   \n \t ", 10, 10), Vec::<&str>::new());
}

#[test]
fn zero_width_clamped_to_one() {
    assert_eq!(lines("ab cd", 0, 0), vec!["a", "b", "c", "d"]);
}

#[test]
fn single_word_wider_than_both_widths() {
    assert_eq!(lines("abcdef", 2, 3), vec!["ab", "cde", "f"]);
}
