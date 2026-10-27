//! One proxy for everything tuinews fetches: feeds, pictures and the
//! articles you open.
//!
//! reqwest reads the proxy variables its own way (`HTTPS_PROXY` for https
//! but never `HTTP_PROXY`), so a proxy meant to hide your address could be
//! skipped without a word. So tuinews picks one at start, from the first of
//! [`NAMES`] that's set, and hands it to reqwest explicitly; with none set,
//! nothing uses a proxy, not even one in the system's settings.
//!
//! Only `http://` and `https://` proxies can be used, so any other kind (a
//! SOCKS one) is refused at start rather than gone around. The proxy's
//! address may hold a password, so it's never printed.

use std::ffi::OsString;
use std::sync::OnceLock;

/// The variables a proxy is taken from, first set first.
pub const NAMES: [&str; 6] = [
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
    "http_proxy",
    "HTTP_PROXY",
];

/// Where the proxy came from, and its address (`http://…`, maybe with a
/// password).
#[derive(Clone, Debug, PartialEq)]
pub struct Proxy {
    pub from: &'static str,
    pub url: String,
}

static CHOSEN: OnceLock<Option<Proxy>> = OnceLock::new();

/// Picks the proxy, once, at start. An error names the variable but never
/// its value.
pub fn init() -> anyhow::Result<()> {
    let chosen = choose(|name| std::env::var_os(name))?;
    let _ = CHOSEN.set(chosen);
    Ok(())
}

/// The proxy in use, if any. Before [`init`] (tests): none.
pub fn get() -> Option<&'static Proxy> {
    CHOSEN.get().and_then(Option::as_ref)
}

fn choose(var: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<Option<Proxy>> {
    // One that isn't text is set all the same: refused below, never read as
    // none, which would send everything direct.
    let Some((from, value)) = NAMES.iter().find_map(|&name| {
        let value = var(name)?;
        let set = value.to_str().is_none_or(|v| !v.trim().is_empty());
        set.then_some((name, value))
    }) else {
        return Ok(None);
    };
    let Some(value) = value.to_str() else {
        anyhow::bail!("{from} doesn't hold a proxy address tuinews can read");
    };
    let value = value.trim();
    // `host:port` means an http proxy, as curl reads it.
    let url = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let parsed = reqwest::Url::parse(&url)
        .map_err(|_| anyhow::anyhow!("{from} doesn't hold a proxy address tuinews can read"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        anyhow::bail!(
            "{from} names a {} proxy; tuinews can only use an http:// or https:// one, \
             and won't go around it",
            parsed.scheme()
        );
    }
    Ok(Some(Proxy { from, url }))
}

/// The proxy for showing: where it's from and its host, without a password.
pub fn shown() -> String {
    match get() {
        None => "none (direct)".into(),
        Some(proxy) => {
            let shown = reqwest::Url::parse(&proxy.url)
                .ok()
                .map(|u| {
                    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
                    format!("{}://{}{port}", u.scheme(), u.host_str().unwrap_or(""))
                })
                .unwrap_or_default();
            crate::text::clean(&format!("{shown} (from {})", proxy.from))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn the_first_set_variable_is_the_proxy_for_everything() {
        assert_eq!(choose(env(&[])).unwrap(), None);
        assert_eq!(choose(env(&[("NO_PROXY", "*")])).unwrap(), None);
        let both = [("HTTPS_PROXY", "http://a:1"), ("https_proxy", "http://b:2")];
        assert_eq!(choose(env(&both)).unwrap().unwrap().url, "http://b:2");
        // An http proxy alone is used for https too.
        let http = [("HTTP_PROXY", "proxy.example:3128")];
        assert_eq!(
            choose(env(&http)).unwrap(),
            Some(Proxy {
                from: "HTTP_PROXY",
                url: "http://proxy.example:3128".into()
            })
        );
    }

    #[test]
    fn a_proxy_that_cant_be_used_is_refused_without_showing_it() {
        let error = choose(env(&[("ALL_PROXY", "socks5h://u:secret@127.0.0.1:9050")]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("ALL_PROXY") && error.contains("http://"),
            "{error}"
        );
        assert!(
            !error.contains("secret") && !error.contains("9050"),
            "{error}"
        );
        let bad = choose(env(&[("https_proxy", "http://")])).unwrap_err();
        assert!(bad.to_string().contains("https_proxy"), "{bad}");
    }

    #[cfg(unix)]
    #[test]
    fn a_proxy_that_isnt_text_is_refused_not_taken_for_none() {
        use std::os::unix::ffi::OsStringExt;
        let garbled = |name: &str| {
            (name == "https_proxy").then(|| OsString::from_vec(b"http://pr\xffoxy:3128".to_vec()))
        };
        let error = choose(garbled).unwrap_err().to_string();
        assert!(error.contains("https_proxy"), "{error}");
    }
}
