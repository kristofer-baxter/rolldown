use std::borrow::Cow;

use oxc::ast::{
  Comment,
  ast::{Expression, NewExpression},
};
use rolldown_common::{ImportKind, ImportRecordMeta, ModuleType, get_leading_comment};
use rolldown_ecmascript_utils::ExpressionExt;
use rolldown_utils::{dataurl::is_data_url, ecmascript::to_usv_str, url::clean_url};

use super::AstScanner;

/// Decode valid percent escapes in the path of a URL-like specifier.
/// Escapes for reserved characters and backslashes stay encoded to preserve path structure.
/// The query and fragment stay unchanged.
/// Returns `None` when nothing changes or the decoded bytes are not valid UTF-8.
fn decode_uri_path(v: &str) -> Option<String> {
  let path = clean_url(v);
  if !path.contains('%') {
    return None;
  }

  let bytes = path.as_bytes();
  let mut decoded = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    let byte = bytes[i];
    if byte == b'%'
      && let Some(value) =
        bytes.get(i + 1..i + 3).filter(|hex| hex.iter().all(u8::is_ascii_hexdigit)).and_then(
          |hex| std::str::from_utf8(hex).ok().and_then(|hex| u8::from_str_radix(hex, 16).ok()),
        )
      && !b";/?:@&=+$,#\\".contains(&value)
    {
      decoded.push(value);
      i += 3;
    } else {
      decoded.push(byte);
      i += 1;
    }
  }

  if decoded == bytes {
    return None;
  }
  let mut decoded = String::from_utf8(decoded).ok()?;
  decoded.push_str(&v[path.len()..]);
  Some(decoded)
}

impl<'me, 'ast: 'me> AstScanner<'me, 'ast> {
  /// Handle exact `new URL('path', import.meta.url)` pattern
  pub fn handle_new_url_with_string_literal_and_import_meta_url(
    &mut self,
    expr: &NewExpression<'ast>,
  ) {
    let is_callee_global_url = matches!(expr.callee.as_identifier(), Some(ident) if ident.name == "URL" && self.is_global_identifier_reference(ident));

    if !is_callee_global_url {
      return;
    }

    let is_second_arg_import_meta_url = expr
      .arguments
      .get(1)
      .is_some_and(|arg| arg.as_expression().is_some_and(ExpressionExt::is_import_meta_url));

    if !is_second_arg_import_meta_url {
      return;
    }

    let Some(first_arg) = expr.arguments.first().and_then(|arg| arg.as_expression()) else {
      return;
    };

    let (path, first_arg_span) = match first_arg {
      Expression::StringLiteral(lit) => (to_usv_str(lit.value), lit.span),
      Expression::TemplateLiteral(tpl) if tpl.is_no_substitution_template() => {
        let Some(value) = tpl.quasis[0].value.cooked else {
          return;
        };
        (to_usv_str(value), tpl.span)
      }
      _ => return,
    };

    let has_leading_ignore_comment = get_leading_comment(
      self.immutable_ctx.comments,
      first_arg_span,
      Some(|comment: &Comment| comment.is_vite()),
    )
    .is_some();
    if has_leading_ignore_comment {
      return;
    }

    if is_data_url(&path) {
      return;
    }

    // `new URL()` treats its first argument as a URL, so `my%20file.txt` refers to `my file.txt`.
    let path = decode_uri_path(&path).map_or(path, Cow::Owned);

    let idx = self.add_import_record(
      &path,
      ImportKind::NewUrl,
      first_arg_span,
      expr.span,
      ImportRecordMeta::empty(),
      None,
    );
    self.result.import_records[idx].asserted_module_type = Some(ModuleType::Asset);
    self.result.new_url_references.insert(expr.node_id(), idx);
  }
}

#[cfg(test)]
mod tests {
  use super::decode_uri_path;

  #[test]
  fn decodes_like_decode_uri() {
    assert_eq!(decode_uri_path("./my%20file.txt").as_deref(), Some("./my file.txt"));
    assert_eq!(decode_uri_path("./%E2%9C%93.txt").as_deref(), Some("./\u{2713}.txt"));
    assert_eq!(decode_uri_path("./100%25.txt").as_deref(), Some("./100%.txt"));
    // Reserved characters keep their escapes, as with `decodeURI`.
    assert_eq!(decode_uri_path("./a%2Fb%20c.txt").as_deref(), Some("./a%2Fb c.txt"));
    assert_eq!(decode_uri_path("./a%3Fb.txt"), None);
    // The query and fragment are left as written.
    assert_eq!(decode_uri_path("./a%20b.txt?x=%20#y%20").as_deref(), Some("./a b.txt?x=%20#y%20"));
  }

  #[test]
  fn leaves_input_without_valid_escapes() {
    assert_eq!(decode_uri_path("./plain.txt"), None);
    assert_eq!(decode_uri_path("./a.txt?x=%20"), None);
    assert_eq!(decode_uri_path("./100%.txt"), None);
    assert_eq!(decode_uri_path("./%zz.txt"), None);
    // A lone UTF-8 continuation byte is not valid UTF-8.
    assert_eq!(decode_uri_path("./%80.txt"), None);
  }

  #[test]
  fn leaves_plus_prefixed_escape() {
    assert_eq!(decode_uri_path("./a%+5.txt"), None);
  }

  #[test]
  fn leaves_encoded_backslash() {
    assert_eq!(decode_uri_path("./a%5Cb.txt"), None);
  }
}
