//! Zero-dependency recursive descent S-expression parser.
//!
//! Grammar: `sexpr = atom | '(' sexpr* ')'`
//! - Atoms: bare words (`foo`, `123`) or `"quoted strings"` with `\"` / `\\` escapes
//! - `;` starts a line comment (to end of line)
//! - Whitespace is insignificant outside quotes

use std::fmt;

/// A parsed S-expression: either an atom (string) or a list of sub-expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SExpr {
    Atom(String),
    List(Vec<SExpr>),
}

impl SExpr {
    /// Return the atom value, or `None` if this is a list.
    pub fn as_atom(&self) -> Option<&str> {
        match self {
            SExpr::Atom(s) => Some(s),
            SExpr::List(_) => None,
        }
    }

    /// Return the list contents, or `None` if this is an atom.
    pub fn as_list(&self) -> Option<&[SExpr]> {
        match self {
            SExpr::Atom(_) => None,
            SExpr::List(v) => Some(v),
        }
    }
}

impl fmt::Display for SExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SExpr::Atom(s) => {
                if s.contains(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '"') {
                    write!(f, "\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
                } else {
                    write!(f, "{}", s)
                }
            }
            SExpr::List(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, ")")
            }
        }
    }
}

/// Parse error with position information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub position: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parse error at position {}: {}",
            self.position, self.message
        )
    }
}

impl std::error::Error for ParseError {}

/// Parse an S-expression from input text.
///
/// The input should contain exactly one top-level expression (typically a list).
pub fn parse(input: &str) -> Result<SExpr, ParseError> {
    let mut pos = 0;
    skip_ws_and_comments(input, &mut pos);
    if pos >= input.len() {
        return Err(ParseError {
            message: "empty input".into(),
            position: 0,
        });
    }
    let expr = parse_sexpr(input, &mut pos)?;
    skip_ws_and_comments(input, &mut pos);
    if pos < input.len() {
        return Err(ParseError {
            message: "unexpected trailing input".into(),
            position: pos,
        });
    }
    Ok(expr)
}

fn parse_sexpr(input: &str, pos: &mut usize) -> Result<SExpr, ParseError> {
    skip_ws_and_comments(input, pos);
    if *pos >= input.len() {
        return Err(ParseError {
            message: "unexpected end of input".into(),
            position: *pos,
        });
    }

    let ch = input.as_bytes()[*pos];
    if ch == b'(' {
        parse_list(input, pos)
    } else if ch == b')' {
        Err(ParseError {
            message: "unexpected ')'".into(),
            position: *pos,
        })
    } else {
        parse_atom(input, pos)
    }
}

fn parse_list(input: &str, pos: &mut usize) -> Result<SExpr, ParseError> {
    let open_pos = *pos;
    *pos += 1; // skip '('
    let mut items = Vec::new();
    loop {
        skip_ws_and_comments(input, pos);
        if *pos >= input.len() {
            return Err(ParseError {
                message: "unmatched '('".into(),
                position: open_pos,
            });
        }
        if input.as_bytes()[*pos] == b')' {
            *pos += 1; // skip ')'
            return Ok(SExpr::List(items));
        }
        items.push(parse_sexpr(input, pos)?);
    }
}

fn parse_atom(input: &str, pos: &mut usize) -> Result<SExpr, ParseError> {
    if input.as_bytes()[*pos] == b'"' {
        parse_quoted_string(input, pos)
    } else {
        parse_bare_word(input, pos)
    }
}

fn parse_quoted_string(input: &str, pos: &mut usize) -> Result<SExpr, ParseError> {
    let start = *pos;
    *pos += 1; // skip opening '"'
    let mut s = String::new();
    let bytes = input.as_bytes();
    while *pos < bytes.len() {
        let ch = bytes[*pos];
        if ch == b'\\' {
            *pos += 1;
            if *pos >= bytes.len() {
                return Err(ParseError {
                    message: "unterminated escape in string".into(),
                    position: start,
                });
            }
            match bytes[*pos] {
                b'"' => s.push('"'),
                b'\\' => s.push('\\'),
                b'n' => s.push('\n'),
                b't' => s.push('\t'),
                other => {
                    s.push('\\');
                    s.push(other as char);
                }
            }
        } else if ch == b'"' {
            *pos += 1; // skip closing '"'
            return Ok(SExpr::Atom(s));
        } else {
            s.push(ch as char);
        }
        *pos += 1;
    }
    Err(ParseError {
        message: "unterminated string".into(),
        position: start,
    })
}

fn parse_bare_word(input: &str, pos: &mut usize) -> Result<SExpr, ParseError> {
    let start = *pos;
    let bytes = input.as_bytes();
    while *pos < bytes.len() {
        let ch = bytes[*pos];
        if ch.is_ascii_whitespace() || ch == b'(' || ch == b')' || ch == b';' || ch == b'"' {
            break;
        }
        *pos += 1;
    }
    if *pos == start {
        return Err(ParseError {
            message: "expected atom".into(),
            position: start,
        });
    }
    Ok(SExpr::Atom(input[start..*pos].to_string()))
}

fn skip_ws_and_comments(input: &str, pos: &mut usize) {
    let bytes = input.as_bytes();
    while *pos < bytes.len() {
        if bytes[*pos].is_ascii_whitespace() {
            *pos += 1;
        } else if bytes[*pos] == b';' {
            // Skip to end of line.
            while *pos < bytes.len() && bytes[*pos] != b'\n' {
                *pos += 1;
            }
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bare_atom() {
        assert_eq!(parse("hello").unwrap(), SExpr::Atom("hello".into()));
    }

    #[test]
    fn test_quoted_string() {
        assert_eq!(
            parse(r#""hello world""#).unwrap(),
            SExpr::Atom("hello world".into())
        );
    }

    #[test]
    fn test_quoted_string_escapes() {
        assert_eq!(
            parse(r#""say \"hi\" \\""#).unwrap(),
            SExpr::Atom(r#"say "hi" \"#.into())
        );
    }

    #[test]
    fn test_simple_list() {
        let expr = parse("(a b c)").unwrap();
        let items = expr.as_list().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].as_atom().unwrap(), "a");
        assert_eq!(items[1].as_atom().unwrap(), "b");
        assert_eq!(items[2].as_atom().unwrap(), "c");
    }

    #[test]
    fn test_nested_lists() {
        let expr = parse("(a (b c) (d (e)))").unwrap();
        let items = expr.as_list().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].as_atom().unwrap(), "a");
        let inner1 = items[1].as_list().unwrap();
        assert_eq!(inner1.len(), 2);
        let inner2 = items[2].as_list().unwrap();
        assert_eq!(inner2.len(), 2);
    }

    #[test]
    fn test_comments() {
        let input = "; this is a comment\n(a ; inline comment\n b)";
        let expr = parse(input).unwrap();
        let items = expr.as_list().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_atom().unwrap(), "a");
        assert_eq!(items[1].as_atom().unwrap(), "b");
    }

    #[test]
    fn test_empty_list() {
        let expr = parse("()").unwrap();
        assert_eq!(expr.as_list().unwrap().len(), 0);
    }

    #[test]
    fn test_unmatched_open_paren() {
        let err = parse("(a b").unwrap_err();
        assert!(err.message.contains("unmatched '('"), "{}", err);
    }

    #[test]
    fn test_unmatched_close_paren() {
        let err = parse("a)").unwrap_err();
        assert!(
            err.message.contains("trailing input") || err.message.contains("unexpected"),
            "{}",
            err
        );
    }

    #[test]
    fn test_unterminated_string() {
        let err = parse(r#""hello"#).unwrap_err();
        assert!(err.message.contains("unterminated"), "{}", err);
    }

    #[test]
    fn test_empty_input() {
        let err = parse("").unwrap_err();
        assert!(err.message.contains("empty"), "{}", err);
    }

    #[test]
    fn test_comment_only_input() {
        let err = parse("; just a comment\n").unwrap_err();
        assert!(err.message.contains("empty"), "{}", err);
    }

    #[test]
    fn test_keyword_atoms() {
        let expr = parse("(:ready-port 5432)").unwrap();
        let items = expr.as_list().unwrap();
        assert_eq!(items[0].as_atom().unwrap(), ":ready-port");
        assert_eq!(items[1].as_atom().unwrap(), "5432");
    }

    #[test]
    fn test_display_round_trip() {
        let input = "(compose (service db (image \"postgres:16\")))";
        let expr = parse(input).unwrap();
        let printed = expr.to_string();
        let reparsed = parse(&printed).unwrap();
        assert_eq!(expr, reparsed);
    }

    #[test]
    fn test_full_compose_example() {
        let input = r#"
; A typical web application stack
(compose
  (network backend (subnet "10.88.1.0/24"))
  (volume pgdata)

  (service db
    (image "postgres:16")
    (network backend)
    (volume pgdata "/var/lib/postgresql/data")
    (env POSTGRES_PASSWORD "secret")
    (port 5432 5432)
    (memory "512m"))

  (service api
    (image "my-api:latest")
    (network backend)
    (depends-on (db :ready-port 5432))
    (port 8080 8080))

  (service web
    (image "my-web:latest")
    (depends-on (api :ready-port 8080))
    (port 80 3000)
    (command "/bin/sh" "-c" "nginx -g 'daemon off;'")))
"#;
        let expr = parse(input).unwrap();
        let items = expr.as_list().unwrap();
        assert_eq!(items[0].as_atom().unwrap(), "compose");
        // network, volume, 3 services = 5 items + "compose" = 6
        assert_eq!(items.len(), 6);
    }

    #[test]
    fn test_as_atom_on_list() {
        let expr = parse("(a)").unwrap();
        assert!(expr.as_atom().is_none());
    }

    #[test]
    fn test_as_list_on_atom() {
        let expr = parse("hello").unwrap();
        assert!(expr.as_list().is_none());
    }
}
