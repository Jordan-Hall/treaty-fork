//! Port of Angular's `directive_matching.ts`: CSS selectors (`CssSelector`), the selector trie
//! (`SelectorMatcher`) and its list and `:not` contexts.
//!
//! This replaces the reduced engine the binder used to carry (which matched attribute NAMES only, so
//! `input[type=number]` matched every `input` with a `type` attribute, and `:not([type=checkbox])`
//! could not be honoured). The matching semantics here are Angular's, including attribute VALUES, the
//! `*` wildcard buckets and the comma-separated selector list that reports a directive once.
//!
//! The node-side selector ([`css_selector_from_node`]) mirrors `createCssSelectorFromNode` in
//! `render3/view/util.ts`: static attributes carry their value, bound inputs and outputs carry an empty
//! value, and a structural template's own attributes carry an empty value under the `ng-template` element.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

/// `RegExp` `\w` (ASCII word characters) plus the `-` Angular allows in selector names.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// A tokenised selector piece, as Angular's `_SELECTOR_REGEXP` groups report it.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// `:not(`
    Not,
    /// `tag`, `.class` or `#id`: the prefix (`'.'`, `'#'` or `'\0'`) and the full text including it.
    Tag { prefix: char, text: String },
    /// `[name]`, `[name=value]`, `[name="value"]`, `[name='value']`.
    Attribute { name: String, value: String },
    /// `)`
    NotEnd,
    /// `\s*,\s*`
    Separator,
}

/// Scan `selector` as Angular's global `_SELECTOR_REGEXP` does: at each position try the alternatives in
/// order, and skip a character that starts none of them.
fn tokenize(selector: &str) -> Vec<Token> {
    let chars: Vec<char> = selector.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // `:not(`
        if chars[i..].starts_with(&[':', 'n', 'o', 't', '(']) {
            tokens.push(Token::Not);
            i += 5;
            continue;
        }
        // `([\.\#]?)[-\w]+`
        let (prefix, start) = if (chars[i] == '.' || chars[i] == '#')
            && chars.get(i + 1).is_some_and(|c| is_name_char(*c))
        {
            (chars[i], i)
        } else if is_name_char(chars[i]) {
            ('\0', i)
        } else {
            ('\0', usize::MAX)
        };
        if start != usize::MAX {
            let name_start = if prefix == '\0' { i } else { i + 1 };
            let mut j = name_start;
            while j < chars.len() && is_name_char(chars[j]) {
                j += 1;
            }
            let text: String = chars[i..j].iter().collect();
            tokens.push(Token::Tag { prefix, text });
            i = j;
            continue;
        }
        // `\[([-.\w*\\$]+)(?:=(["']?)([^\]"']*)\2)?\]`
        if chars[i] == '[' {
            if let Some((token, end)) = attribute_at(&chars, i) {
                tokens.push(token);
                i = end;
                continue;
            }
        }
        if chars[i] == ')' {
            tokens.push(Token::NotEnd);
            i += 1;
            continue;
        }
        if chars[i].is_whitespace() || chars[i] == ',' {
            // `\s*,\s*`: only a comma makes a separator; leading whitespace before a non-comma is skipped.
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && chars[j] == ',' {
                j += 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                tokens.push(Token::Separator);
                i = j;
            } else {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    tokens
}

/// Match one `[...]` attribute alternative starting at `start` (which is `[`). Returns the token and the
/// index just past the closing `]`, or `None` when the text is not a complete attribute selector.
fn attribute_at(chars: &[char], start: usize) -> Option<(Token, usize)> {
    let mut i = start + 1;
    let name_start = i;
    while i < chars.len() && (is_name_char(chars[i]) || matches!(chars[i], '.' | '*' | '\\' | '$')) {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name: String = chars[name_start..i].iter().collect();
    if i < chars.len() && chars[i] == ']' {
        return Some((Token::Attribute { name, value: String::new() }, i + 1));
    }
    if i >= chars.len() || chars[i] != '=' {
        return None;
    }
    i += 1;
    let quote = match chars.get(i) {
        Some('"') | Some('\'') => {
            let q = chars[i];
            i += 1;
            Some(q)
        }
        _ => None,
    };
    let value_start = i;
    while i < chars.len() && !matches!(chars[i], ']' | '"' | '\'') {
        i += 1;
    }
    let value: String = chars[value_start..i].iter().collect();
    if let Some(q) = quote {
        if chars.get(i) != Some(&q) {
            return None;
        }
        i += 1;
    }
    if chars.get(i) != Some(&']') {
        return None;
    }
    Some((Token::Attribute { name, value }, i + 1))
}

/// A CSS selector: an element name, class names and attribute/value pairs, plus `:not(...)` groups.
/// Mirrors Angular's `CssSelector`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CssSelector {
    pub element: Option<String>,
    pub class_names: Vec<String>,
    /// The attribute selectors in pairs: even indices are attribute names, odd indices their values
    /// (lowercased; `''` for a name with no value). `[key1=value1][key2]` is `["key1", "value1", "key2", ""]`.
    pub attrs: Vec<String>,
    pub not_selectors: Vec<CssSelector>,
}

impl CssSelector {
    /// Angular's `CssSelector.parse`: one selector per comma-separated group.
    pub fn parse(selector: &str) -> Result<Vec<CssSelector>, String> {
        let mut results: Vec<CssSelector> = Vec::new();
        let add_result = |results: &mut Vec<CssSelector>, mut css: CssSelector| {
            if !css.not_selectors.is_empty()
                && css.element.is_none()
                && css.class_names.is_empty()
                && css.attrs.is_empty()
            {
                css.element = Some("*".to_string());
            }
            results.push(css);
        };
        let mut css = CssSelector::default();
        let mut in_not = false;
        // `current` is the selector a token writes into: the top-level one, or the `:not(...)` group.
        // The `:not` group is held as the last entry of `css.not_selectors` while `in_not` is set.
        for token in tokenize(selector) {
            match token {
                Token::Not => {
                    if in_not {
                        return Err("Nesting :not in a selector is not allowed".to_string());
                    }
                    in_not = true;
                    css.not_selectors.push(CssSelector::default());
                }
                Token::Tag { prefix, text } => {
                    let current = Self::current(&mut css, in_not);
                    match prefix {
                        '#' => current.add_attribute("id", &text[1..]),
                        '.' => current.add_class_name(&text[1..]),
                        _ => current.element = Some(text),
                    }
                }
                Token::Attribute { name, value } => {
                    let unescaped = unescape_attribute(&name)?;
                    Self::current(&mut css, in_not).add_attribute(&unescaped, &value);
                }
                Token::NotEnd => {
                    in_not = false;
                }
                Token::Separator => {
                    if in_not {
                        return Err("Multiple selectors in :not are not supported".to_string());
                    }
                    add_result(&mut results, std::mem::take(&mut css));
                }
            }
        }
        add_result(&mut results, css);
        Ok(results)
    }

    /// The selector a token writes into (the `:not` group while one is open, else the top level).
    fn current(css: &mut CssSelector, in_not: bool) -> &mut CssSelector {
        if in_not {
            css.not_selectors.last_mut().expect("a :not group is open")
        } else {
            css
        }
    }

    /// Angular's `addAttribute(name, value = '')`: the value is stored lowercased.
    pub fn add_attribute(&mut self, name: &str, value: &str) {
        self.attrs.push(name.to_string());
        self.attrs.push(value.to_lowercase());
    }

    /// Angular's `addClassName`: stored lowercased.
    pub fn add_class_name(&mut self, name: &str) {
        self.class_names.push(name.to_lowercase());
    }

    pub fn has_element_selector(&self) -> bool {
        self.element.is_some()
    }
}

/// Angular's `unescapeAttribute`: `\x` yields `x`; an unescaped `$` is an error.
fn unescape_attribute(attr: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut escaping = false;
    for c in attr.chars() {
        if c == '\\' {
            escaping = true;
            continue;
        }
        if c == '$' && !escaping {
            return Err(format!(
                "Error in attribute selector \"{attr}\". Unescaped \"$\" is not supported. Please escape with \"\\$\"."
            ));
        }
        escaping = false;
        result.push(c);
    }
    Ok(result)
}

/// Angular's `SelectorListContext`: the directive a comma-separated selector list belongs to, reported once.
#[derive(Debug, Default)]
pub struct SelectorListContext {
    already_matched: Cell<bool>,
}

/// One registered selector with its callback context (Angular's `SelectorContext`).
pub struct SelectorContext<T> {
    selector: CssSelector,
    cb_context: T,
    list_context: Option<Rc<SelectorListContext>>,
}

impl<T> SelectorContext<T> {
    /// Angular's `finalize`: a selector with `:not` groups matches only when none of them match, and the
    /// callback runs once per list context. A match is pushed to `out`; the return value is Angular's
    /// `result` (a list that already reported its directive still counts as matched).
    fn finalize<'a>(&'a self, css: &CssSelector, out: &mut Vec<(&'a CssSelector, &'a T)>) -> bool {
        let mut result = true;
        let open = self
            .list_context
            .as_ref()
            .is_none_or(|list| !list.already_matched.get());
        if !self.selector.not_selectors.is_empty() && open {
            let not_matcher = SelectorMatcher::<()>::create_not_matcher(&self.selector.not_selectors);
            result = !not_matcher.matches(css);
        }
        if result && open {
            if let Some(list) = &self.list_context {
                list.already_matched.set(true);
            }
            out.push((&self.selector, &self.cb_context));
        }
        result
    }
}

type Contexts<T> = Vec<Rc<SelectorContext<T>>>;

/// Angular's `SelectorMatcher`: a trie over the element, class and attribute-value parts of the registered
/// selectors. Build it with [`SelectorMatcher::add_selectables`] and query it with
/// [`SelectorMatcher::match_selector`].
pub struct SelectorMatcher<T> {
    element_map: HashMap<String, Contexts<T>>,
    element_partial_map: HashMap<String, SelectorMatcher<T>>,
    class_map: HashMap<String, Contexts<T>>,
    class_partial_map: HashMap<String, SelectorMatcher<T>>,
    attr_value_map: HashMap<String, HashMap<String, Contexts<T>>>,
    attr_value_partial_map: HashMap<String, HashMap<String, SelectorMatcher<T>>>,
    list_contexts: Vec<Rc<SelectorListContext>>,
}

impl<T> Default for SelectorMatcher<T> {
    fn default() -> Self {
        SelectorMatcher {
            element_map: HashMap::new(),
            element_partial_map: HashMap::new(),
            class_map: HashMap::new(),
            class_partial_map: HashMap::new(),
            attr_value_map: HashMap::new(),
            attr_value_partial_map: HashMap::new(),
            list_contexts: Vec::new(),
        }
    }
}

impl<T: Clone> SelectorMatcher<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Angular's `createNotMatcher`: the matcher of a selector's `:not` groups.
    fn create_not_matcher(not_selectors: &[CssSelector]) -> SelectorMatcher<()> {
        let mut matcher = SelectorMatcher::<()>::new();
        matcher.add_selectables(not_selectors, ());
        matcher
    }

    /// Angular's `addSelectables(cssSelectors, callbackCtxt)`: register every selector of one comma list for
    /// the same callback context. A list of more than one selector shares a list context, so the callback
    /// runs once even when several of its selectors match.
    pub fn add_selectables(&mut self, css_selectors: &[CssSelector], cb_context: T) {
        let list_context = if css_selectors.len() > 1 {
            let list = Rc::new(SelectorListContext::default());
            self.list_contexts.push(list.clone());
            Some(list)
        } else {
            None
        };
        for css in css_selectors {
            self.add_selectable(css, cb_context.clone(), list_context.clone());
        }
    }

    /// Angular's `_addSelectable`: walk (and create) the nested partial matchers for each part of the
    /// selector, storing the context in the terminal bucket of its last part.
    fn add_selectable(
        &mut self,
        css: &CssSelector,
        cb_context: T,
        list_context: Option<Rc<SelectorListContext>>,
    ) {
        let selectable = Rc::new(SelectorContext {
            selector: css.clone(),
            cb_context,
            list_context,
        });
        let mut matcher: &mut SelectorMatcher<T> = self;

        if let Some(element) = &css.element {
            let is_terminal = css.attrs.is_empty() && css.class_names.is_empty();
            if is_terminal {
                matcher
                    .element_map
                    .entry(element.clone())
                    .or_default()
                    .push(selectable.clone());
            } else {
                matcher = matcher
                    .element_partial_map
                    .entry(element.clone())
                    .or_default();
            }
        }

        for (i, class_name) in css.class_names.iter().enumerate() {
            let is_terminal = css.attrs.is_empty() && i == css.class_names.len() - 1;
            if is_terminal {
                matcher
                    .class_map
                    .entry(class_name.clone())
                    .or_default()
                    .push(selectable.clone());
            } else {
                matcher = matcher
                    .class_partial_map
                    .entry(class_name.clone())
                    .or_default();
            }
        }

        let pairs = css.attrs.len() / 2;
        for pair in 0..pairs {
            let name = &css.attrs[pair * 2];
            let value = &css.attrs[pair * 2 + 1];
            let is_terminal = pair == pairs - 1;
            if is_terminal {
                matcher
                    .attr_value_map
                    .entry(name.clone())
                    .or_default()
                    .entry(value.clone())
                    .or_default()
                    .push(selectable.clone());
            } else {
                matcher = matcher
                    .attr_value_partial_map
                    .entry(name.clone())
                    .or_default()
                    .entry(value.clone())
                    .or_default();
            }
        }
    }

    /// Angular's `match(cssSelector, matchedCallback)`: append `(selector, context)` for every registered
    /// selector the node's selector matches, and return whether any matched (Angular's boolean result).
    pub fn match_selector<'a>(&'a self, css: &CssSelector, out: &mut Vec<(&'a CssSelector, &'a T)>) -> bool {
        for list in &self.list_contexts {
            list.already_matched.set(false);
        }
        let mut result = false;
        let element = css.element.as_deref().unwrap_or("");
        result = self.match_terminal(&self.element_map, element, css, out) || result;
        result = self.match_partial(&self.element_partial_map, element, css, out) || result;

        for class_name in &css.class_names {
            result = self.match_terminal(&self.class_map, class_name, css, out) || result;
            result = self.match_partial(&self.class_partial_map, class_name, css, out) || result;
        }

        for pair in 0..css.attrs.len() / 2 {
            let name = &css.attrs[pair * 2];
            let value = &css.attrs[pair * 2 + 1];
            let terminal_values = self.attr_value_map.get(name);
            let partial_values = self.attr_value_partial_map.get(name);
            // A non-empty value also matches the empty-value selectors of the same name (`[attr]`).
            if !value.is_empty() {
                result = self.match_terminal_values(terminal_values, "", css, out) || result;
            }
            result = self.match_terminal_values(terminal_values, value, css, out) || result;
            if !value.is_empty() {
                result = self.match_partial_values(partial_values, "", css, out) || result;
            }
            result = self.match_partial_values(partial_values, value, css, out) || result;
        }
        result
    }

    /// Whether any registered selector matches, without collecting the matches.
    pub fn matches(&self, css: &CssSelector) -> bool {
        let mut out = Vec::new();
        self.match_selector(css, &mut out)
    }

    fn match_terminal<'a>(
        &'a self,
        map: &'a HashMap<String, Contexts<T>>,
        name: &str,
        css: &CssSelector,
        out: &mut Vec<(&'a CssSelector, &'a T)>,
    ) -> bool {
        self.match_terminal_values(Some(map), name, css, out)
    }

    /// Angular's `_matchTerminal`: the bucket of `name` plus the `*` wildcard bucket.
    fn match_terminal_values<'a>(
        &'a self,
        map: Option<&'a HashMap<String, Contexts<T>>>,
        name: &str,
        css: &CssSelector,
        out: &mut Vec<(&'a CssSelector, &'a T)>,
    ) -> bool {
        let Some(map) = map else { return false };
        let selectables = map
            .get(name)
            .into_iter()
            .flatten()
            .chain(map.get("*").into_iter().flatten());
        let mut result = false;
        for selectable in selectables {
            result = selectable.finalize(css, out) || result;
        }
        result
    }

    fn match_partial<'a>(
        &'a self,
        map: &'a HashMap<String, SelectorMatcher<T>>,
        name: &str,
        css: &CssSelector,
        out: &mut Vec<(&'a CssSelector, &'a T)>,
    ) -> bool {
        match map.get(name) {
            Some(nested) => nested.match_selector(css, out),
            None => false,
        }
    }

    fn match_partial_values<'a>(
        &'a self,
        map: Option<&'a HashMap<String, SelectorMatcher<T>>>,
        name: &str,
        css: &CssSelector,
        out: &mut Vec<(&'a CssSelector, &'a T)>,
    ) -> bool {
        match map {
            Some(map) => self.match_partial(map, name, css, out),
            None => false,
        }
    }
}

/// The node-side selector of an element, as Angular's `createCssSelectorFromNode` builds it for an
/// `Element`: the element name, then every attribute keyed by name (a static attribute keeps its value, a
/// bound property or two-way binding and an output carry `''`; a later key overwrites an earlier one in
/// place), with each `class` token recorded as a class name.
pub fn css_selector_from_node(
    element: Option<&str>,
    attributes: &[(String, String)],
    bound_inputs: &[String],
    outputs: &[String],
) -> CssSelector {
    let mut map: Vec<(String, String)> = Vec::new();
    fn set(map: &mut Vec<(String, String)>, name: &str, value: &str) {
        match map.iter_mut().find(|(k, _)| k == name) {
            Some(entry) => entry.1 = value.to_string(),
            None => map.push((name.to_string(), value.to_string())),
        }
    }
    for (name, value) in attributes {
        if is_i18n_attribute(name) {
            continue;
        }
        set(&mut map, name, value);
    }
    for name in bound_inputs {
        set(&mut map, name, "");
    }
    for name in outputs {
        set(&mut map, name, "");
    }
    build_css(element.unwrap_or(""), map)
}

/// The node-side selector of a structural template (`<ng-template>` or `*directive` host), as Angular builds
/// it: `ng-template` is the element, and an element-backed template contributes its own attributes.
pub fn css_selector_from_template(
    tag_name: Option<&str>,
    attributes: &[(String, String)],
    bound_inputs: &[String],
    outputs: &[String],
    template_attrs: &[String],
) -> CssSelector {
    // Angular: `elOrTpl instanceof Template && (!tagName || !isNgTemplate(tagName))` reads the template's
    // own attributes (all `''`); an `<ng-template>` reads its element attributes, inputs and outputs.
    let use_template_attrs = tag_name.is_none_or(|t| t != "ng-template");
    let mut map: Vec<(String, String)> = Vec::new();
    fn set(map: &mut Vec<(String, String)>, name: &str, value: &str) {
        match map.iter_mut().find(|(k, _)| k == name) {
            Some(entry) => entry.1 = value.to_string(),
            None => map.push((name.to_string(), value.to_string())),
        }
    }
    if use_template_attrs {
        for name in template_attrs {
            set(&mut map, name, "");
        }
    } else {
        for (name, value) in attributes {
            if is_i18n_attribute(name) {
                continue;
            }
            set(&mut map, name, value);
        }
        for name in bound_inputs {
            set(&mut map, name, "");
        }
        for name in outputs {
            set(&mut map, name, "");
        }
    }
    build_css("ng-template", map)
}

fn build_css(element: &str, map: Vec<(String, String)>) -> CssSelector {
    let mut css = CssSelector {
        element: Some(split_ns_name(element).to_string()),
        ..CssSelector::default()
    };
    for (name, value) in map {
        let name_no_ns = split_ns_name(&name);
        css.add_attribute(name_no_ns, &value);
        if name.to_lowercase() == "class" {
            for class_name in value.trim().split_whitespace() {
                css.add_class_name(class_name);
            }
        }
    }
    css
}

/// `splitNsName`: `:svg:rect` → `rect`; a name without a namespace prefix is returned as is.
fn split_ns_name(name: &str) -> &str {
    match name.strip_prefix(':') {
        Some(rest) => rest.split_once(':').map(|(_, local)| local).unwrap_or(rest),
        None => name,
    }
}

/// Angular's `isI18nAttribute`: `i18n` or `i18n-<name>`.
fn is_i18n_attribute(name: &str) -> bool {
    name == "i18n" || name.starts_with("i18n-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(element: &str, attrs: &[(&str, &str)]) -> CssSelector {
        let attributes: Vec<(String, String)> = attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        css_selector_from_node(Some(element), &attributes, &[], &[])
    }

    fn matches(selector: &str, css: &CssSelector) -> bool {
        let mut matcher = SelectorMatcher::<usize>::new();
        matcher.add_selectables(&CssSelector::parse(selector).expect("parse"), 0);
        matcher.matches(css)
    }

    #[test]
    fn parses_element_class_id_and_attribute_pairs() {
        let parsed = CssSelector::parse("div.Foo#bar[role=button][disabled]").unwrap();
        assert_eq!(parsed.len(), 1);
        let css = &parsed[0];
        assert_eq!(css.element.as_deref(), Some("div"));
        assert_eq!(css.class_names, vec!["foo"]);
        // `#bar` is the `id` attribute; values are lowercased; a valueless attribute is ''.
        assert_eq!(css.attrs, vec!["id", "bar", "role", "button", "disabled", ""]);
    }

    #[test]
    fn splits_comma_groups_and_keeps_quoted_values() {
        let parsed = CssSelector::parse("a[href='x'], [routerLink]").unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].attrs, vec!["href", "x"]);
        // Attribute NAMES keep their case (only values and class names are lowercased, as Angular does).
        assert_eq!(parsed[1].attrs, vec!["routerLink", ""]);
    }

    #[test]
    fn not_groups_are_parsed_and_a_bare_not_is_the_star_element() {
        let parsed = CssSelector::parse(":not(span)").unwrap();
        assert_eq!(parsed[0].element.as_deref(), Some("*"));
        assert_eq!(parsed[0].not_selectors.len(), 1);
        assert_eq!(parsed[0].not_selectors[0].element.as_deref(), Some("span"));
        let mixed = CssSelector::parse("input:not([type=checkbox])").unwrap();
        assert_eq!(mixed[0].element.as_deref(), Some("input"));
        assert_eq!(mixed[0].not_selectors[0].attrs, vec!["type", "checkbox"]);
    }

    #[test]
    fn invalid_selectors_are_errors_as_in_angular() {
        assert!(CssSelector::parse(":not(a:not(b))").is_err());
        assert!(CssSelector::parse(":not(a, b)").is_err());
        assert!(CssSelector::parse("[a$b]").is_err(), "an unescaped $ is not allowed");
        assert!(CssSelector::parse("[a\\$b]").is_ok());
    }

    /// The defect this module fixes: `input[type=number]` must not match a text input.
    #[test]
    fn attribute_values_are_compared() {
        let text_input = node("input", &[("type", "text"), ("ngModel", "")]);
        assert!(!matches("input[type=number]", &text_input));
        assert!(matches("input[type=text]", &text_input));
        assert!(matches("input[type=TEXT]", &text_input), "values compare lowercased");
    }

    /// `DefaultValueAccessor`'s selector: a `:not` value test on a list of attribute selectors.
    #[test]
    fn not_with_attribute_value_gates_the_match() {
        let selector = "input:not([type=checkbox])[ngModel],textarea[ngModel],[ngDefaultControl]";
        let text_input = node("input", &[("type", "text"), ("ngModel", "")]);
        let checkbox = node("input", &[("type", "checkbox"), ("ngModel", "")]);
        assert!(matches(selector, &text_input));
        assert!(!matches(selector, &checkbox));
    }

    #[test]
    fn bound_inputs_match_by_name_with_an_empty_value() {
        let css = css_selector_from_node(Some("input"), &[], &["formControl".to_string()], &[]);
        assert!(matches("[formControl]", &css));
        assert!(!matches("[formControl=x]", &css));
    }

    #[test]
    fn class_selectors_match_the_class_attribute_tokens() {
        let css = node("div", &[("class", "  card  shadow ")]);
        assert!(matches(".card", &css));
        assert!(matches("div.card.shadow", &css));
        assert!(!matches(".missing", &css));
    }

    #[test]
    fn a_selector_list_reports_its_directive_once() {
        let mut matcher = SelectorMatcher::<&str>::new();
        let list = CssSelector::parse("a, [x]").unwrap();
        matcher.add_selectables(&list, "dir");
        let css = node("a", &[("x", "")]);
        let mut out = Vec::new();
        matcher.match_selector(&css, &mut out);
        let hits: Vec<&str> = out.iter().map(|(_, cb)| **cb).collect();
        assert_eq!(hits, vec!["dir"], "both selectors of the list match, the directive is reported once");
    }

    #[test]
    fn star_bucket_matches_any_element() {
        let mut matcher = SelectorMatcher::<&str>::new();
        matcher.add_selectables(&CssSelector::parse(":not(span)").unwrap(), "any-but-span");
        assert!(matcher.matches(&node("div", &[])));
        assert!(!matcher.matches(&node("span", &[])));
    }

    #[test]
    fn template_nodes_match_as_ng_template_with_their_template_attributes() {
        let css = css_selector_from_template(Some("div"), &[], &[], &[], &["ngIf".to_string()]);
        assert_eq!(css.element.as_deref(), Some("ng-template"));
        assert!(matches("[ngIf]", &css));
        assert!(matches("ng-template[ngIf]", &css));
        assert!(!matches("[ngif]", &css), "attribute names are case-sensitive");
    }

    #[test]
    fn namespaced_element_names_are_matched_without_their_prefix() {
        let css = node(":svg:rect", &[]);
        assert_eq!(css.element.as_deref(), Some("rect"));
    }
}
