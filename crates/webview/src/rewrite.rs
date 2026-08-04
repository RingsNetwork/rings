use std::cell::Cell;
use std::cell::RefCell;
use std::error::Error;

use lol_html::element;
use lol_html::end;
use lol_html::errors::RewritingError;
use lol_html::html_content::ContentType;
use lol_html::html_content::Element;
use lol_html::html_content::TextChunk;
use lol_html::text;
use lol_html::HandlerTypes;
use lol_html::HtmlRewriter;
use lol_html::RewriteStrSettings;
use url::Url;

use crate::error::Result;
use crate::error::WebviewError;
use crate::url::GatewayPrefix;

mod budget;
mod srcset;

use self::budget::bounded_value;
use self::budget::response_body_too_large;
use self::budget::BoundedString;
use self::srcset::visit_srcset_candidates;
use self::srcset::SrcsetCandidate;

const MAX_SRCDOC_NESTING_DEPTH: u8 = 8;

/// Context required to rewrite one target document or stylesheet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewriteContext {
    gateway_prefix: GatewayPrefix,
    document_url: Url,
    bootstrap_script: Option<String>,
    bootstrap_serialization: BootstrapSerialization,
    srcdoc_depth: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum BootstrapSerialization {
    #[default]
    Html,
    Xhtml,
}

impl RewriteContext {
    /// Build a rewrite context for `document_url`.
    pub fn new(gateway_prefix: GatewayPrefix, document_url: Url) -> Self {
        Self {
            gateway_prefix,
            document_url,
            bootstrap_script: None,
            bootstrap_serialization: BootstrapSerialization::Html,
            srcdoc_depth: 0,
        }
    }

    /// Attach a bootstrap runtime script to inject into HTML documents.
    pub fn with_bootstrap_script(mut self, script: impl Into<String>) -> Self {
        self.bootstrap_script = Some(script.into());
        self.bootstrap_serialization = BootstrapSerialization::Html;
        self
    }

    /// Attach a bootstrap runtime to an XHTML document without permitting a second-root
    /// fallback. The raw script remains available to nested HTML `srcdoc` rewrites.
    pub(crate) fn with_xhtml_bootstrap_script(mut self, script: impl Into<String>) -> Self {
        self.bootstrap_script = Some(script.into());
        self.bootstrap_serialization = BootstrapSerialization::Xhtml;
        self
    }

    /// Rewrite one HTML document body.
    pub fn rewrite_html(&self, html: &str) -> Result<String> {
        self.rewrite_html_with_limit(html, usize::MAX)
    }

    pub(crate) fn rewrite_html_with_limit(&self, html: &str, limit: usize) -> Result<String> {
        let bootstrap = self
            .bootstrap_script
            .as_ref()
            .map(|script| bootstrap_tag(script, self.bootstrap_serialization));
        let injected = Cell::new(bootstrap.is_none());
        let allow_document_fallback = self.bootstrap_serialization == BootstrapSerialization::Html;
        let inject_into_body = self.bootstrap_serialization == BootstrapSerialization::Xhtml;
        let html_state = HtmlRewriteState::new(
            &self.gateway_prefix,
            self.document_url.clone(),
            self.bootstrap_script.as_deref(),
            limit,
            self.srcdoc_depth,
        );
        let style_text = RefCell::new(String::new());
        let settings = RewriteStrSettings::new()
            .append_element_content_handler(element!("*", |element| {
                rewrite_html_element(element, &html_state)?;
                if !injected.get() {
                    if let Some(tag) = bootstrap.as_deref() {
                        if element.tag_name().eq_ignore_ascii_case("head")
                            || (inject_into_body && element.tag_name().eq_ignore_ascii_case("body"))
                        {
                            element.prepend(tag, ContentType::Html);
                            injected.set(true);
                        } else if element.tag_name().eq_ignore_ascii_case("script") {
                            element.before(tag, ContentType::Html);
                            injected.set(true);
                        }
                    }
                }
                Ok(())
            }))
            .append_element_content_handler(text!("style", |text| {
                rewrite_style_text(text, &html_state, &style_text)?;
                Ok(())
            }))
            .append_document_content_handler(end!(|end| {
                if allow_document_fallback && !injected.get() {
                    if let Some(tag) = bootstrap.as_deref() {
                        end.append(tag, ContentType::Html);
                        injected.set(true);
                    }
                }
                Ok(())
            }));
        let mut output = Vec::with_capacity(html.len().min(limit));
        let mut overflow_actual = None;
        {
            let mut rewriter = HtmlRewriter::new(settings.into(), |chunk: &[u8]| {
                if overflow_actual.is_some() {
                    return;
                }
                let actual = output.len().saturating_add(chunk.len());
                if actual > limit {
                    overflow_actual = Some(actual);
                } else {
                    output.extend_from_slice(chunk);
                }
            });
            rewriter
                .write(html.as_bytes())
                .and_then(|()| rewriter.end())
                .map_err(map_html_rewrite_error)?;
        }
        if let Some(actual) = overflow_actual {
            return Err(response_body_too_large(actual, limit));
        }
        String::from_utf8(output).map_err(|error| {
            WebviewError::Render(format!("HTML rewrite emitted invalid UTF-8: {error}"))
        })
    }

    /// Rewrite one CSS stylesheet body.
    pub fn rewrite_css(&self, css: &str) -> Result<String> {
        self.rewrite_css_with_limit(css, usize::MAX)
    }

    pub(crate) fn rewrite_css_with_limit(&self, css: &str, limit: usize) -> Result<String> {
        let imports = rewrite_css_imports(css, self, limit)?;
        rewrite_css_urls(&imports, self, limit)
    }

    fn rewrite_url(&self, value: &str) -> Result<Option<String>> {
        self.gateway_prefix
            .rewrite_url_value(&self.document_url, value)
    }
}

/// Preserve typed handler failures while projecting parser/serializer failures into the WebView
/// rendering error algebra. This belongs to the HTML adapter, not the pure output-budget module.
fn map_html_rewrite_error(error: RewritingError) -> WebviewError {
    match error {
        RewritingError::ContentHandlerError(source) => match source.downcast::<WebviewError>() {
            Ok(error) => *error,
            Err(source) => WebviewError::Render(format!("HTML rewrite handler failed: {source}")),
        },
        error => WebviewError::Render(format!("HTML rewrite failed: {error}")),
    }
}

struct HtmlRewriteState<'a> {
    gateway_prefix: &'a GatewayPrefix,
    current_base: RefCell<Url>,
    base_href_seen: Cell<bool>,
    bootstrap_script: Option<&'a str>,
    output_limit: usize,
    srcdoc_depth: u8,
}

impl<'a> HtmlRewriteState<'a> {
    fn new(
        gateway_prefix: &'a GatewayPrefix,
        document_url: Url,
        bootstrap_script: Option<&'a str>,
        output_limit: usize,
        srcdoc_depth: u8,
    ) -> Self {
        Self {
            gateway_prefix,
            current_base: RefCell::new(document_url),
            base_href_seen: Cell::new(false),
            bootstrap_script,
            output_limit,
            srcdoc_depth,
        }
    }

    fn rewrite_url(&self, value: &str) -> Result<Option<String>> {
        let base = self.current_base.borrow();
        self.gateway_prefix
            .rewrite_url_value(&base, value)?
            .map(|rewritten| bounded_value(rewritten, self.output_limit))
            .transpose()
    }

    fn rewrite_base_href(&self, value: &str) -> Result<Option<String>> {
        let base = self.current_base.borrow().clone();
        let Some(target) = self.gateway_prefix.resolve_url_value(&base, value)? else {
            return Ok(None);
        };
        if !self.base_href_seen.get() {
            self.current_base.replace(target.clone());
            self.base_href_seen.set(true);
        }
        bounded_value(self.gateway_prefix.encode(&target), self.output_limit).map(Some)
    }

    fn rewrite_css(&self, css: &str) -> Result<String> {
        let base = self.current_base.borrow().clone();
        RewriteContext::new(self.gateway_prefix.clone(), base)
            .rewrite_css_with_limit(css, self.output_limit)
    }

    fn rewrite_srcdoc(&self, html: &str) -> Result<String> {
        if self.srcdoc_depth >= MAX_SRCDOC_NESTING_DEPTH {
            return Err(WebviewError::SrcdocNestingLimit {
                max_depth: MAX_SRCDOC_NESTING_DEPTH,
            });
        }
        let base = self.current_base.borrow().clone();
        let mut context = RewriteContext::new(self.gateway_prefix.clone(), base);
        context.srcdoc_depth = self.srcdoc_depth + 1;
        if let Some(script) = self.bootstrap_script {
            context = context.with_bootstrap_script(script);
        }
        context.rewrite_html_with_limit(
            decode_srcdoc_attribute_value(html).as_str(),
            self.output_limit,
        )
    }

    fn rewrite_refresh(&self, value: &str) -> Result<String> {
        let base = self.current_base.borrow();
        rewrite_refresh_value_with_limit(value, &base, self.gateway_prefix, self.output_limit)
    }
}

fn rewrite_html_element<H>(
    element: &mut Element<'_, '_, H>,
    state: &HtmlRewriteState<'_>,
) -> std::result::Result<(), Box<dyn Error + Send + Sync>>
where
    H: HandlerTypes,
{
    let tag_name = element.tag_name();
    let is_base = tag_name.eq_ignore_ascii_case("base");
    let attributes: Vec<(String, String)> = element
        .attributes()
        .iter()
        .map(|attribute| (attribute.name(), attribute.value()))
        .collect();
    let is_meta_refresh = tag_name.eq_ignore_ascii_case("meta")
        && attributes.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("http-equiv") && value.trim().eq_ignore_ascii_case("refresh")
        });
    for (name, value) in attributes {
        let lower_name = name.to_ascii_lowercase();
        if is_base && lower_name == "href" {
            if let Some(rewritten) = state.rewrite_base_href(value.as_str())? {
                element.set_attribute(name.as_str(), rewritten.as_str())?;
            }
        } else if lower_name == "srcdoc" {
            let rewritten = state.rewrite_srcdoc(value.as_str())?;
            element.set_attribute(name.as_str(), rewritten.as_str())?;
        } else if lower_name == "ping" {
            let rewritten = rewrite_url_list_value(value.as_str(), state)?;
            element.set_attribute(name.as_str(), rewritten.as_str())?;
        } else if is_meta_refresh && lower_name == "content" {
            let rewritten = state.rewrite_refresh(value.as_str())?;
            element.set_attribute(name.as_str(), rewritten.as_str())?;
        } else if is_url_attribute(lower_name.as_str()) {
            if let Some(rewritten) = state.rewrite_url(value.as_str())? {
                element.set_attribute(name.as_str(), rewritten.as_str())?;
            }
        } else if is_srcset_attribute(lower_name.as_str()) {
            let rewritten = rewrite_srcset_value(value.as_str(), state)?;
            element.set_attribute(name.as_str(), rewritten.as_str())?;
        } else if lower_name == "style" {
            let rewritten = state.rewrite_css(value.as_str())?;
            element.set_attribute(name.as_str(), rewritten.as_str())?;
        }
    }
    Ok(())
}

fn rewrite_style_text(
    text: &mut TextChunk<'_>,
    state: &HtmlRewriteState<'_>,
    buffer: &RefCell<String>,
) -> std::result::Result<(), Box<dyn Error + Send + Sync>> {
    let mut buffered = buffer.borrow_mut();
    buffered.push_str(text.as_str());
    if !text.last_in_text_node() {
        text.replace("", ContentType::Text);
        return Ok(());
    }

    let css = std::mem::take(&mut *buffered);
    drop(buffered);
    let rewritten = state.rewrite_css(css.as_str())?;
    text.replace(rewritten.as_str(), ContentType::Text);
    Ok(())
}

fn is_url_attribute(name: &str) -> bool {
    matches!(
        name,
        "href"
            | "src"
            | "action"
            | "poster"
            | "data"
            | "cite"
            | "formaction"
            | "manifest"
            | "xlink:href"
    )
}

fn is_srcset_attribute(name: &str) -> bool {
    matches!(name, "srcset" | "imagesrcset")
}

fn rewrite_srcset_value(value: &str, state: &HtmlRewriteState<'_>) -> Result<String> {
    let mut out = BoundedString::new(value.len(), state.output_limit);
    let mut first = true;
    visit_srcset_candidates(value, |SrcsetCandidate { url, descriptor }| {
        let rewritten = state.rewrite_url(&url)?.unwrap_or(url);
        if !first {
            out.push_str(", ")?;
        }
        first = false;
        out.push_str(&rewritten)?;
        if !descriptor.is_empty() {
            out.push(' ')?;
            out.push_str(&descriptor)?;
        }
        Ok(())
    })?;
    Ok(out.finish())
}

/// Rewrite a space-separated HTML URL list, such as an anchor `ping` value.
fn rewrite_url_list_value(value: &str, state: &HtmlRewriteState<'_>) -> Result<String> {
    let mut rewritten = BoundedString::new(value.len(), state.output_limit);
    let mut first = true;
    for candidate in value.split_whitespace() {
        if !first {
            rewritten.push(' ')?;
        }
        first = false;
        let candidate = state
            .rewrite_url(candidate)?
            .unwrap_or_else(|| candidate.to_string());
        rewritten.push_str(&candidate)?;
    }
    Ok(rewritten.finish())
}

fn rewrite_css_urls(input: &str, ctx: &RewriteContext, limit: usize) -> Result<String> {
    let mut output = BoundedString::new(input.len(), limit);
    let mut rest = input;
    while let Some(index) = find_ascii_case_insensitive(rest, "url(") {
        let (before, after_url) = split_at_checked(rest, index)?;
        let (url_token, after_open) = split_at_checked(after_url, "url(".len())?;
        output.push_str(before)?;
        output.push_str(url_token)?;
        let close_index = match find_css_url_end(after_open) {
            CssUrlEnd::Close(index) => index,
            CssUrlEnd::Nested(index) => {
                let (malformed, nested) = split_at_checked(after_open, index)?;
                output.push_str(malformed)?;
                rest = nested;
                continue;
            }
            CssUrlEnd::Missing => {
                output.push_str(after_open)?;
                return Ok(output.finish());
            }
        };
        let (raw_value, close_and_tail) = split_at_checked(after_open, close_index)?;
        let (_, tail) = split_at_checked(close_and_tail, ')'.len_utf8())?;
        let (quote, value) = trim_css_url(raw_value);
        if let Some(rewritten) = ctx.rewrite_url(value)? {
            if let Some(quote) = quote {
                output.push(quote)?;
                output.push_str(rewritten.as_str())?;
                output.push(quote)?;
            } else {
                output.push_str(rewritten.as_str())?;
            }
        } else {
            output.push_str(raw_value)?;
        }
        output.push(')')?;
        rest = tail;
    }
    output.push_str(rest)?;
    Ok(output.finish())
}

enum CssUrlEnd {
    Close(usize),
    Nested(usize),
    Missing,
}

fn find_css_url_end(input: &str) -> CssUrlEnd {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if quote.is_some_and(|expected| expected == character) {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if quote.is_none() && character == ')' {
            return CssUrlEnd::Close(index);
        } else if quote.is_none()
            && index > 0
            && input.get(index..).is_some_and(|tail| {
                tail.get(..4)
                    .is_some_and(|token| token.eq_ignore_ascii_case("url("))
            })
        {
            return CssUrlEnd::Nested(index);
        }
    }
    CssUrlEnd::Missing
}

fn trim_css_url(raw_value: &str) -> (Option<char>, &str) {
    let trimmed = raw_value.trim();
    if let Some(value) = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        (Some('"'), value)
    } else if let Some(value) = trimmed
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        (Some('\''), value)
    } else {
        (None, trimmed)
    }
}

fn rewrite_css_imports(input: &str, ctx: &RewriteContext, limit: usize) -> Result<String> {
    let mut output = BoundedString::new(input.len(), limit);
    let mut rest = input;
    while let Some(index) = find_ascii_case_insensitive(rest, "@import") {
        let (before, after_import) = split_at_checked(rest, index)?;
        let (import_token, after_token) = split_at_checked(after_import, "@import".len())?;
        output.push_str(before)?;
        output.push_str(import_token)?;
        let whitespace_len = after_token
            .bytes()
            .take_while(u8::is_ascii_whitespace)
            .count();
        let (whitespace, after_whitespace) = split_at_checked(after_token, whitespace_len)?;
        output.push_str(whitespace)?;
        let Some(quote) = after_whitespace
            .chars()
            .next()
            .filter(|ch| matches!(ch, '"' | '\''))
        else {
            rest = after_whitespace;
            continue;
        };
        let Some(after_quote) = after_whitespace.strip_prefix(quote) else {
            output.push_str(after_whitespace)?;
            return Ok(output.finish());
        };
        let Some((value, tail)) = after_quote.split_once(quote) else {
            output.push(quote)?;
            output.push_str(after_quote)?;
            return Ok(output.finish());
        };
        output.push(quote)?;
        if let Some(rewritten) = ctx.rewrite_url(value)? {
            output.push_str(rewritten.as_str())?;
        } else {
            output.push_str(value)?;
        }
        output.push(quote)?;
        rest = tail;
    }
    output.push_str(rest)?;
    Ok(output.finish())
}

/// Rewrite a Refresh header or meta refresh content value through the gateway.
pub(crate) fn rewrite_refresh_value(
    value: &str,
    base_url: &Url,
    gateway_prefix: &GatewayPrefix,
) -> Result<String> {
    rewrite_refresh_value_with_limit(value, base_url, gateway_prefix, usize::MAX)
}

fn rewrite_refresh_value_with_limit(
    value: &str,
    base_url: &Url,
    gateway_prefix: &GatewayPrefix,
    limit: usize,
) -> Result<String> {
    let Some(url_index) = find_refresh_url_key(value) else {
        return Ok(value.to_string());
    };
    let (before_url, after_url) = split_at_checked(value, url_index)?;
    let (url_token, after_token) = split_at_checked(after_url, "url".len())?;
    let whitespace_len = after_token
        .bytes()
        .take_while(u8::is_ascii_whitespace)
        .count();
    let (whitespace, after_whitespace) = split_at_checked(after_token, whitespace_len)?;
    let Some(after_equals) = after_whitespace.strip_prefix('=') else {
        return Ok(value.to_string());
    };
    let value_whitespace_len = after_equals
        .bytes()
        .take_while(u8::is_ascii_whitespace)
        .count();
    let (value_whitespace, raw_target) = split_at_checked(after_equals, value_whitespace_len)?;
    let (quote, target, suffix) = split_refresh_target(raw_target);
    let Some(rewritten) = gateway_prefix.rewrite_url_value(base_url, target)? else {
        return Ok(value.to_string());
    };

    let mut output = BoundedString::new(value.len(), limit);
    output.push_str(before_url)?;
    output.push_str(url_token)?;
    output.push_str(whitespace)?;
    output.push('=')?;
    output.push_str(value_whitespace)?;
    if let Some(quote) = quote {
        output.push(quote)?;
        output.push_str(rewritten.as_str())?;
        output.push(quote)?;
    } else {
        output.push_str(rewritten.as_str())?;
    }
    output.push_str(suffix)?;
    Ok(output.finish())
}

fn find_refresh_url_key(value: &str) -> Option<usize> {
    let mut rest = value;
    let mut offset = 0_usize;
    while let Some(index) = find_ascii_case_insensitive(rest, "url") {
        let absolute_index = offset.checked_add(index)?;
        let after_url = rest.get(index.checked_add("url".len())?..)?;
        let whitespace_len = after_url
            .bytes()
            .take_while(u8::is_ascii_whitespace)
            .count();
        if after_url
            .get(whitespace_len..)
            .is_some_and(|tail| tail.starts_with('='))
        {
            return Some(absolute_index);
        }
        let advance = index.checked_add("url".len())?;
        rest = rest.get(advance..)?;
        offset = offset.checked_add(advance)?;
    }
    None
}

fn split_refresh_target(raw_target: &str) -> (Option<char>, &str, &str) {
    if let Some(quote) = raw_target
        .chars()
        .next()
        .filter(|ch| matches!(ch, '"' | '\''))
    {
        if let Some(after_quote) = raw_target.strip_prefix(quote) {
            if let Some((target, suffix)) = after_quote.split_once(quote) {
                return (Some(quote), target, suffix);
            }
            return (Some(quote), after_quote, "");
        }
    }

    let target = raw_target.trim_end();
    let suffix = raw_target.get(target.len()..).unwrap_or_default();
    (None, target, suffix)
}

fn decode_srcdoc_attribute_value(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&#X22;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&#X27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn find_ascii_case_insensitive(input: &str, pattern: &str) -> Option<usize> {
    let pattern = pattern.as_bytes();
    (!pattern.is_empty()).then_some(())?;
    input
        .as_bytes()
        .windows(pattern.len())
        .position(|candidate| candidate.eq_ignore_ascii_case(pattern))
}

fn split_at_checked(input: &str, index: usize) -> Result<(&str, &str)> {
    let Some(before) = input.get(..index) else {
        return Err(WebviewError::Render(format!(
            "invalid UTF-8 split boundary {index}"
        )));
    };
    let Some(after) = input.get(index..) else {
        return Err(WebviewError::Render(format!(
            "invalid UTF-8 split boundary {index}"
        )));
    };
    Ok((before, after))
}

fn bootstrap_tag(script: &str, serialization: BootstrapSerialization) -> String {
    let source = match serialization {
        BootstrapSerialization::Html => script.to_string(),
        BootstrapSerialization::Xhtml => {
            let escaped = script.replace("]]>", "]]]]><![CDATA[>");
            format!("<![CDATA[\n{escaped}\n]]>")
        }
    };
    format!("<script data-rings-webview-bootstrap>{source}</script>")
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::url::GatewayPrefix;
    use crate::url::TargetUrl;

    fn context() -> Result<RewriteContext> {
        Ok(RewriteContext::new(
            GatewayPrefix::new("/webview/")?,
            TargetUrl::parse("https://example.com/app/page.html")?.into_url(),
        ))
    }

    #[test]
    fn html_rewrites_relative_subresources_and_srcset() -> Result<()> {
        let ctx = context()?;
        let html = r#"<a href="next.html"><img src="/img.png" srcset="small.png 1x, /big.png 2x"><form action='../submit'></form></a>"#;

        let rewritten = ctx.rewrite_html(html)?;

        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Fnext%2Ehtml"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fimg%2Epng"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fsubmit"));
        assert!(rewritten.contains("1x"));
        assert!(rewritten.contains("2x"));
        Ok(())
    }

    #[test]
    fn html_srcset_tokenizer_preserves_data_url_and_candidate_boundaries() -> Result<()> {
        let ctx = context()?;
        let html = r#"<img srcset="data:image/svg+xml,%3Csvg%3E,%3C/svg%3E, /next.png 2x"><img srcset="first.png 1x,,, /last.png 2x">"#;

        let rewritten = ctx.rewrite_html(html)?;

        assert!(rewritten.contains("data:image/svg+xml,%3Csvg%3E,%3C/svg%3E"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fnext%2Epng 2x"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Ffirst%2Epng 1x"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Flast%2Epng 2x"));
        Ok(())
    }

    #[derive(Deserialize)]
    struct SrcsetContractCase {
        name: String,
        input: String,
        rust_output: String,
        candidates: Vec<SrcsetContractCandidate>,
    }

    #[derive(Deserialize)]
    struct SrcsetContractCandidate {
        url: String,
        descriptor: String,
    }

    #[test]
    fn html_srcset_tokenizer_obeys_shared_runtime_contract() -> Result<()> {
        let cases: Vec<SrcsetContractCase> =
            serde_json::from_str(include_str!("srcset_contract.json")).map_err(|error| {
                WebviewError::transport(format!("invalid shared srcset contract: {error}"))
            })?;
        for case in cases {
            let mut actual = Vec::new();
            visit_srcset_candidates(&case.input, |candidate| {
                actual.push(candidate);
                Ok(())
            })?;
            let expected = case
                .candidates
                .into_iter()
                .map(|candidate| SrcsetCandidate {
                    url: candidate.url,
                    descriptor: candidate.descriptor,
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{}", case.name);
            let html = format!(r#"<img srcset='{}'>"#, case.input);
            let rewritten = context()?.rewrite_html(html.as_str())?;
            assert_eq!(
                rewritten,
                format!(r#"<img srcset="{}">"#, case.rust_output),
                "{} rewrite output",
                case.name
            );
        }
        Ok(())
    }

    #[test]
    fn css_rewrites_urls_and_imports() -> Result<()> {
        let ctx = context()?;
        let css = r#"@import "theme/base.css"; body { background: url('/assets/bg.png'); }"#;

        let rewritten = ctx.rewrite_css(css)?;

        assert!(
            rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Ftheme%2Fbase%2Ecss")
        );
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fassets%2Fbg%2Epng"));
        Ok(())
    }

    #[test]
    fn bounded_rewriters_preserve_typed_overflow_for_css_and_html_attributes() -> Result<()> {
        let ctx = context()?;
        for result in [
            ctx.rewrite_css_with_limit("a { background: url('/large.png'); }", 16),
            ctx.rewrite_html_with_limit("<img srcset='/one.png 1x, /two.png 2x'>", 24),
        ] {
            assert!(matches!(
                result,
                Err(WebviewError::Transport(
                    crate::error::TransportFailure::ResponseBodyTooLarge { .. }
                ))
            ));
        }
        Ok(())
    }

    #[test]
    fn bootstrap_injects_before_page_scripts() -> Result<()> {
        let ctx = context()?.with_bootstrap_script("globalThis.__ringsWebview = true;");

        let rewritten = ctx.rewrite_html("<html><script src=\"app.js\"></script></html>")?;

        assert!(rewritten.contains("data-rings-webview-bootstrap"));
        assert!(rewritten.contains("__ringsWebview"));
        Ok(())
    }

    #[test]
    fn html_rewrites_case_whitespace_unquoted_and_inline_style_urls() -> Result<()> {
        let ctx = context()?;
        let html = r#"<A HREF = next.html><img SRC=/img.png SRCSET='small.png 1x, /big.png 2x'><button formaction=../submit></button><div STYLE="background: URL(bg.png)"></div></A>"#;

        let rewritten = ctx.rewrite_html(html)?;

        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Fnext%2Ehtml"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fimg%2Epng"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fsubmit"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Fbg%2Epng"));
        Ok(())
    }

    #[test]
    fn html_rewrites_style_element_urls() -> Result<()> {
        let ctx = context()?;
        let html = r#"<style>@import "theme/base.css"; body { background: url("/assets/bg.png"); }</style>"#;

        let rewritten = ctx.rewrite_html(html)?;

        assert!(
            rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Ftheme%2Fbase%2Ecss")
        );
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fassets%2Fbg%2Epng"));
        assert_absent(&rewritten, "@import \"theme/base.css\"");
        assert_absent(&rewritten, "url(\"/assets/bg.png\")");
        Ok(())
    }

    #[test]
    fn html_rewrites_anchor_ping_url_lists() -> Result<()> {
        let ctx = context()?;
        let html =
            r#"<a href="next.html" ping="ping/one https://metrics.example/ping/two">next</a>"#;

        let rewritten = ctx.rewrite_html(html)?;

        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Fping%2Fone"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fmetrics%2Eexample%2Fping%2Ftwo"));
        assert_absent(&rewritten, "ping/one https://metrics.example/ping/two");
        Ok(())
    }

    #[test]
    fn html_rewrites_srcdoc_document_urls() -> Result<()> {
        let ctx = context()?.with_bootstrap_script("globalThis.__ringsWebview = true;");
        let html = r#"<iframe srcdoc="<img src=&quot;https://example.test/x.png&quot;><a href=&quot;next.html&quot;>next</a>"></iframe>"#;

        let rewritten = ctx.rewrite_html(html)?;

        assert!(
            rewritten.contains("/webview/https%3A%2F%2Fexample%2Etest%2Fx%2Epng"),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Fnext%2Ehtml"),
            "{rewritten}"
        );
        assert!(rewritten.contains("data-rings-webview-bootstrap"));
        assert_absent(&rewritten, "https://example.test/x.png");
        assert_absent(&rewritten, "href=\"next.html\"");
        Ok(())
    }

    #[test]
    fn html_rejects_srcdoc_nesting_beyond_the_explicit_bound() -> Result<()> {
        let ctx = context()?;
        let mut html = "<p>leaf</p>".to_string();
        for _ in 0..=MAX_SRCDOC_NESTING_DEPTH {
            let escaped = html.replace('&', "&amp;").replace('"', "&quot;");
            html = format!(r#"<iframe srcdoc="{escaped}"></iframe>"#);
        }

        assert!(matches!(
            ctx.rewrite_html(&html),
            Err(WebviewError::SrcdocNestingLimit {
                max_depth: MAX_SRCDOC_NESTING_DEPTH
            })
        ));
        Ok(())
    }

    #[test]
    fn html_rewrites_meta_refresh_urls() -> Result<()> {
        let ctx = context()?;
        let html = r#"<meta http-equiv="refresh" content="0; URL = '../login?next=1'">"#;

        let rewritten = ctx.rewrite_html(html)?;

        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Flogin%3Fnext%3D1"));
        assert_absent(&rewritten, "../login?next=1");
        Ok(())
    }

    #[test]
    fn html_base_href_controls_following_relative_rewrites() -> Result<()> {
        let ctx = context()?;
        let html = r#"<html><head><base href="/assets/"><link href="site.css"><style>.hero { background: url(hero.png); }</style></head><body><img src="photo.png"><script src="app.js"></script></body></html>"#;

        let rewritten = ctx.rewrite_html(html)?;

        for expected in [
            "https://example.com/assets/",
            "https://example.com/assets/site.css",
            "https://example.com/assets/hero.png",
            "https://example.com/assets/photo.png",
            "https://example.com/assets/app.js",
        ] {
            assert!(rewritten.contains(
                GatewayPrefix::new("/webview/")?
                    .encode(&Url::parse(expected)?)
                    .as_str()
            ));
        }
        assert_absent(&rewritten, "href=\"site.css\"");
        assert_absent(&rewritten, "url(hero.png)");
        assert_absent(&rewritten, "src=\"photo.png\"");
        Ok(())
    }

    #[test]
    fn css_rewrites_case_whitespace_and_import_url_forms() -> Result<()> {
        let ctx = context()?;
        let css = r#"@IMPORT    "theme/base.css"; @import url(extra.css); body { background: URL("/assets/bg.png"); }"#;

        let rewritten = ctx.rewrite_css(css)?;

        assert!(
            rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Ftheme%2Fbase%2Ecss")
        );
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Fextra%2Ecss"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fassets%2Fbg%2Epng"));
        Ok(())
    }

    #[test]
    fn css_url_scanner_preserves_quoted_parentheses_and_rewrites_later_urls() -> Result<()> {
        let ctx = context()?;
        let css = r#".a { background: url("a)b.png"); } .b { background: url(/later.png); }"#;

        let rewritten = ctx.rewrite_css(css)?;

        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Fapp%2Fa%29b%2Epng"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Flater%2Epng"));
        Ok(())
    }

    #[test]
    fn malformed_css_url_does_not_suppress_later_independent_rewrite() -> Result<()> {
        let ctx = context()?;
        let css = ".broken { background: url('unterminated'; } .ok { mask: URL(/later.svg); }";

        let rewritten = ctx.rewrite_css(css)?;

        assert!(rewritten.contains("url('unterminated'"));
        assert!(rewritten.contains("/webview/https%3A%2F%2Fexample%2Ecom%2Flater%2Esvg"));
        Ok(())
    }

    fn assert_absent(haystack: &str, needle: &str) {
        assert!(
            !haystack.contains(needle),
            "unexpected substring {needle:?} in {haystack:?}"
        );
    }
}
