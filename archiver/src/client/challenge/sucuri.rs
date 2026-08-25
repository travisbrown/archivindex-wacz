//! Recognition of Sucuri `CloudProxy`'s JavaScript cookie challenge.
//!
//! The challenge page carries a base64-encoded script that sets one cookie and reloads. The
//! decoder below reads that script's cookie assignment under a grammar narrow enough that
//! nothing else can be expressed in it: literals, character codes, and one variable.

use archivindex_warc::recorder::CapturedExchange;
use data_encoding::BASE64;
use http::StatusCode;
use http::header::HeaderValue;

use super::StoredCookie;

/// Decode the cookie from Sucuri's JavaScript reload challenge without evaluating its script.
pub fn challenge_cookie(captured: &CapturedExchange) -> Option<StoredCookie> {
    if captured.response_metadata.status != StatusCode::TEMPORARY_REDIRECT.as_u16()
        || captured.response_metadata.header("x-sucuri-id").is_none()
    {
        return None;
    }

    parse_cookie(&captured.entity_body().ok()?)
}

fn parse_cookie(body: &[u8]) -> Option<StoredCookie> {
    let html = std::str::from_utf8(body).ok()?;
    let encoded = html.split_once("sucuri_cloudproxy_js=")?.1.trim_start();
    let (encoded, rest) = parse_js_string(encoded)?;
    let encoded = if encoded.is_empty() {
        parse_js_string(rest.trim_start().strip_prefix(",S=")?.trim_start())?.0
    } else {
        encoded
    };
    let decoded = BASE64.decode(encoded.as_bytes()).ok()?;
    let script = std::str::from_utf8(&decoded).ok()?.trim_start();

    let (variable_name, value_expression) = script.split_once('=')?;
    if variable_name.is_empty()
        || variable_name.len() > 16
        || !variable_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
        || variable_name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_digit)
    {
        return None;
    }
    let (challenge_value, _) = evaluate_js_statement(value_expression, None)?;
    let cookie_expression = script.split_once("document.cookie=")?.1;
    let (cookie, _) =
        evaluate_js_statement(cookie_expression, Some((variable_name, &challenge_value)))?;
    let mut attributes = cookie.split(';');
    let pair = attributes.next()?.trim();
    let (name, value) = pair.split_once('=')?;
    if !name.starts_with("sucuri_cloudproxy_uuid_")
        || name.len() == "sucuri_cloudproxy_uuid_".len()
        || value.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }

    Some(StoredCookie {
        value: HeaderValue::from_str(pair).ok()?,
        secure: attributes.any(|attribute| attribute.trim().eq_ignore_ascii_case("secure")),
    })
}

/// Evaluate one semicolon-terminated concatenation of literals, one variable, and character codes.
fn evaluate_js_statement<'a>(
    mut input: &'a str,
    variable: Option<(&str, &str)>,
) -> Option<(String, &'a str)> {
    let mut output = String::new();

    loop {
        input = input.trim_start();
        if input
            .as_bytes()
            .first()
            .is_some_and(|byte| matches!(byte, b'\'' | b'"'))
        {
            let (value, rest) = parse_js_string(input)?;
            output.push_str(&value);
            input = rest;
        } else if let Some(rest) = input.strip_prefix("String.fromCharCode(") {
            let (arguments, rest) = rest.split_once(')')?;
            for argument in arguments.split(',') {
                output.push(char::from_u32(argument.trim().parse().ok()?)?);
            }
            input = rest;
        } else if let Some(rest) = input.strip_prefix("w(") {
            let (arguments, rest) = rest.split_once(')')?;
            for argument in arguments.split(',') {
                output.push(char::from_u32(argument.trim().parse().ok()?)?);
            }
            input = rest;
        } else {
            let (name, value) = variable?;
            let rest = input.strip_prefix(name)?;
            if rest
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
            {
                return None;
            }
            output.push_str(value);
            input = rest;
        }

        input = input.trim_start();
        if let Some(rest) = input.strip_prefix('+') {
            input = rest;
        } else {
            let rest = input.strip_prefix(';')?;
            return Some((output, rest));
        }
    }
}

/// Parse the restricted single- or double-quoted string literals used by Sucuri's generator.
fn parse_js_string(input: &str) -> Option<(String, &str)> {
    let bytes = input.as_bytes();
    let quote = *bytes.first()?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }

    let mut output = String::new();
    let mut index = 1;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == quote {
            return Some((output, &input[index + 1..]));
        }
        if byte == b'\\' {
            index += 1;
            let escaped = *bytes.get(index)?;
            output.push(match escaped {
                b'\\' => '\\',
                b'\'' => '\'',
                b'"' => '"',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                _ => return None,
            });
        } else if byte.is_ascii() {
            output.push(char::from(byte));
        } else {
            return None;
        }
        index += 1;
    }

    None
}

#[cfg(test)]
mod tests {
    use data_encoding::BASE64;

    use super::parse_cookie;

    #[test]
    fn cookie_decoder_handles_obfuscated_concatenation_without_evaluation() {
        let script = "r='b' + String.fromCharCode(51) + 'f';\
            document.cookie='s'+'u'+'c'+'u'+'r'+'i'+'_'+'c'+'l'+'o'+'u'+'d'+'p'+'r'+'o'+'x'+\
            'y'+'_'+'u'+'u'+'i'+'d'+'_'+'t'+'e'+'s'+'t'+'=' + r + \
            ';path=/;max-age=86400;SameSite=Lax;Secure'; location.reload();";
        let encoded = BASE64.encode(script.as_bytes());
        let html = format!("<script>sucuri_cloudproxy_js='',S='{encoded}';</script>");

        let cookie = parse_cookie(html.as_bytes()).expect("a recognized Sucuri challenge");

        assert_eq!(cookie.value, "sucuri_cloudproxy_uuid_test=b3f");
        assert!(cookie.secure);
    }

    #[test]
    fn cookie_decoder_rejects_arbitrary_script() {
        let encoded = BASE64.encode(b"fetch(\"https://example.com/\");");
        let html = format!("<script>sucuri_cloudproxy_js='{encoded}';</script>");

        assert!(parse_cookie(html.as_bytes()).is_none());
    }
}
