//! Box names, msb sandbox names and env names.

use crate::fail;

/// `[a-z0-9][a-z0-9-]{0,31}`, and not `t<digits>` (hc's terminal ids).
pub fn check_box(name: &str) -> anyhow::Result<()> {
    let ok_chars = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let first = name.bytes().next();
    let ok_first = first.is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !ok_chars || !ok_first || name.len() > 32 {
        return Err(fail::usage(format!(
            "bad box name '{name}': lowercase letters, digits and '-', starting with a letter or digit, at most 32"
        )));
    }
    if name.len() > 1 && name.starts_with('t') && name[1..].bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail::usage(format!(
            "bad box name '{name}': t<digits> is reserved (hc's terminal ids)"
        )));
    }
    Ok(())
}

/// `instance.prefix`: like a box name, at most 24 bytes, so a msb name
/// stays short. Box names never contain '_', so `<prefix>_builder` can't be a
/// box's msb name.
pub fn check_prefix(prefix: &str) -> Result<(), String> {
    let ok = !prefix.is_empty()
        && prefix.len() <= 24
        && prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && prefix
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "instance.prefix '{prefix}': lowercase letters, digits and '-', starting with a letter or digit, 1 to 24"
        ))
    }
}

/// The msb sandbox name of a box.
pub fn msb_box(prefix: &str, name: &str) -> String {
    format!("{prefix}{name}")
}

/// The msb sandbox name of the instance's builder.
pub fn msb_builder(prefix: &str) -> String {
    format!("{prefix}_builder")
}

/// The msb sandbox name of the box that seeds a new store.
pub fn msb_seed(prefix: &str) -> String {
    format!("{prefix}_seed")
}

/// A POSIX-ish env name.
pub fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// bento's own env names (contracts.md, Inside a box): set by bento in every
/// process it starts, never by the config, `create -e` or `exec -e`. Anything
/// starting with `BENTO_` is bento's too.
pub const RESERVED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "NIX_REMOTE",
    "NIX_CONFIG",
    "NIX_SSL_CERT_FILE",
    "SSL_CERT_FILE",
    "GIT_SSL_CAINFO",
    "GIT_CONFIG_SYSTEM",
    "IS_SANDBOX",
    "TERMINFO_DIRS",
];

pub fn is_reserved_env(name: &str) -> bool {
    RESERVED_ENV.contains(&name) || name.starts_with("BENTO_")
}

/// Parse a `-e K=V`: the name must be a valid env name that isn't bento's.
pub fn parse_env_arg(arg: &str) -> anyhow::Result<(String, String)> {
    let Some((k, v)) = arg.split_once('=') else {
        return Err(fail::usage(format!("-e '{arg}': expected K=V")));
    };
    if !is_env_name(k) {
        return Err(fail::usage(format!("-e '{arg}': '{k}' is not an env name")));
    }
    if is_reserved_env(k) {
        return Err(fail::usage(format!("-e '{arg}': {k} is set by bento")));
    }
    Ok((k.to_string(), v.to_string()))
}

/// Parse an `exec`/`enter` `-e`: `K=V`, or `K` alone (Docker's convention),
/// which takes K's value from `caller` (the caller's environment); K unset
/// there: None, the variable isn't set by this layer. Errors about a `K`
/// alone name only K, never its value.
pub fn parse_exec_env_arg(
    arg: &str,
    caller: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> anyhow::Result<Option<(String, String)>> {
    if arg.contains('=') {
        return parse_env_arg(arg).map(Some);
    }
    if !is_env_name(arg) {
        return Err(fail::usage(format!("-e '{arg}': not an env name")));
    }
    if is_reserved_env(arg) {
        return Err(fail::usage(format!("-e {arg}: {arg} is set by bento")));
    }
    match caller(arg) {
        None => Ok(None),
        Some(v) => match v.into_string() {
            Ok(v) => Ok(Some((arg.to_string(), v))),
            Err(_) => Err(fail::usage(format!("-e {arg}: its value isn't UTF-8"))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fail::{code_of, USAGE};

    #[test]
    fn box_names() {
        for good in [
            "a",
            "port-forwarding-k3fq",
            "0x",
            "t",
            "tx1",
            "t1x",
            &"a".repeat(32),
        ] {
            assert!(check_box(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-a",
            "A",
            "a_b",
            "a.b",
            "a/b",
            "../x",
            "t1",
            "t0042",
            &"a".repeat(33),
            "ä",
            "a b",
        ] {
            let err = check_box(bad).unwrap_err();
            assert_eq!(code_of(&err), USAGE, "{bad}");
        }
    }

    #[test]
    fn builder_and_seed_never_clash_with_a_box() {
        let prefix = "bento-";
        for name in ["builder", "seed", "_builder"] {
            let is_box = check_box(name).is_ok();
            assert!(!is_box || msb_box(prefix, name) != msb_builder(prefix));
            assert!(!is_box || msb_box(prefix, name) != msb_seed(prefix));
        }
        assert_eq!(msb_box("bento-", "x"), "bento-x");
        assert_eq!(msb_builder("bento-"), "bento-_builder");
    }

    #[test]
    fn prefixes() {
        assert!(check_prefix("bento-").is_ok());
        assert!(check_prefix("dev1-").is_ok());
        for bad in ["", "-x", "X", "a_b", &"a".repeat(25)] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn env_args() {
        assert_eq!(
            parse_env_arg("A=b=c").unwrap(),
            ("A".to_string(), "b=c".to_string())
        );
        assert_eq!(parse_env_arg("A=").unwrap().1, "");
        for bad in [
            "A",
            "=x",
            "1A=x",
            "A-B=x",
            "PATH=x",
            "HOME=/",
            "BENTO_X=1",
            "NIX_REMOTE=x",
        ] {
            assert_eq!(code_of(&parse_env_arg(bad).unwrap_err()), USAGE, "{bad}");
        }
    }

    #[test]
    fn exec_env_args_from_the_caller() {
        use std::ffi::OsString;
        let caller = |k: &str| match k {
            "TOK" | "PATH" | "1A" => Some(OsString::from("s3cret-value")),
            _ => None,
        };
        let p = |a: &str| parse_exec_env_arg(a, caller);
        assert_eq!(
            p("TOK").unwrap(),
            Some(("TOK".to_string(), "s3cret-value".to_string()))
        );
        assert_eq!(p("UNSET").unwrap(), None, "unset in the caller: not set");
        assert_eq!(p("A=b").unwrap(), Some(("A".to_string(), "b".to_string())));
        for bad in ["PATH", "BENTO_X", "1A", "A-B", ""] {
            let err = p(bad).unwrap_err();
            assert_eq!(code_of(&err), USAGE, "{bad}");
            assert!(
                !format!("{err:#}").contains("s3cret"),
                "{bad}: value in the error"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            let err = parse_exec_env_arg("TOK", |_| Some(OsString::from_vec(vec![0xff, b's'])))
                .unwrap_err();
            assert_eq!(code_of(&err), USAGE);
        }
    }
}
