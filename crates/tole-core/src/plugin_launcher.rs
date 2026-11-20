//! Unpinned launchers (#600, `tole plugin add` and `tole doctor`).
//!
//! A stdio server started through `npx`, `uvx` or `docker run` is whatever
//! the package or image resolves to at that moment. The plugin lock pins
//! the launcher's binary, not the package it fetches, so a command line
//! without a pinned version trusts the registry on every start. This
//! module only reads the command line (no network, no I/O).

/// Why `command args` is an unpinned launcher, or `None` when it is not a
/// known launcher or its package is pinned (an exact version, or a digest
/// for an image).
pub fn unpinned_launcher(command: &str, args: &[String]) -> Option<String> {
    let program = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .trim_end_matches(".cmd")
        .trim_end_matches(".exe");
    let positional = |skip: usize| -> Option<&str> {
        args.iter()
            .map(String::as_str)
            .filter(|a| !a.starts_with('-'))
            .nth(skip)
    };
    match program {
        "npx" | "bunx" | "pnpx" => {
            let pkg = positional(0)?;
            (!npm_pinned(pkg)).then(|| {
                format!("`{program}` runs `{pkg}` at whatever version the registry has now; pin it (`{pkg}@<version>`)")
            })
        }
        "pnpm" | "yarn" | "bun" if args.iter().any(|a| a == "dlx" || a == "x") => {
            let pkg = args
                .iter()
                .skip_while(|a| *a != "dlx" && *a != "x")
                .skip(1)
                .find(|a| !a.starts_with('-'))?;
            (!npm_pinned(pkg)).then(|| format!("`{program} dlx {pkg}` is not pinned to a version"))
        }
        "uvx" | "pipx" => {
            let skip = usize::from(program == "pipx" && args.iter().any(|a| a == "run"));
            let pkg = if program == "pipx" {
                args.iter()
                    .skip_while(|a| *a != "run")
                    .skip(1)
                    .find(|a| !a.starts_with('-'))?
            } else {
                positional(skip)?
            };
            (!py_pinned(pkg)).then(|| {
                format!("`{program}` runs `{pkg}` at whatever version the index has now; pin it (`{pkg}==<version>`)")
            })
        }
        "docker" | "podman" if args.first().map(String::as_str) == Some("run") => {
            let image = image_of(args)?;
            (!image_pinned(image)).then(|| {
                format!("`{program} run` uses image `{image}` without a version tag or digest; pin a tag or `@sha256:...`")
            })
        }
        _ => None,
    }
}

/// `name@1.2.3` or `@scope/name@1.2.3`, not `latest`, `next` or a range.
fn npm_pinned(pkg: &str) -> bool {
    let body = pkg.strip_prefix('@').unwrap_or(pkg);
    match body.rsplit_once('@') {
        Some((_, v)) => exact_version(v),
        None => false,
    }
}

/// `name==1.2.3` or `name@1.2.3`.
fn py_pinned(pkg: &str) -> bool {
    match pkg.split_once("==").or_else(|| pkg.split_once('@')) {
        Some((_, v)) => exact_version(v),
        None => false,
    }
}

fn exact_version(v: &str) -> bool {
    v.chars().next().is_some_and(|c| c.is_ascii_digit())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

/// The image of `docker run [flags] IMAGE ...`. Flags that take a value
/// are skipped with it; an unknown flag makes the answer `None` (no
/// guess), so a launcher is never reported on a misparse.
fn image_of(args: &[String]) -> Option<&str> {
    const WITH_VALUE: &[&str] = &[
        "-e",
        "--env",
        "-v",
        "--volume",
        "-p",
        "--publish",
        "-w",
        "--workdir",
        "-u",
        "--user",
        "--name",
        "--network",
        "--entrypoint",
        "-m",
        "--memory",
        "--mount",
        "--env-file",
        "--platform",
        "--cpus",
    ];
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a.starts_with('-') {
            if a.contains('=') || !WITH_VALUE.contains(&a.as_str()) {
                if !a.contains('=')
                    && !matches!(
                        a.as_str(),
                        "-i" | "-t"
                            | "-it"
                            | "-ti"
                            | "--rm"
                            | "--init"
                            | "-d"
                            | "--read-only"
                            | "--interactive"
                            | "--tty"
                    )
                {
                    return None;
                }
                continue;
            }
            it.next();
            continue;
        }
        return Some(a);
    }
    None
}

fn image_pinned(image: &str) -> bool {
    if image.contains("@sha256:") {
        return true;
    }
    // The tag follows the last `/` (a registry port is before it).
    let name = image.rsplit('/').next().unwrap_or(image);
    match name.rsplit_once(':') {
        Some((_, tag)) => tag != "latest" && !tag.is_empty(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(command: &str, args: &[&str]) -> Option<String> {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        unpinned_launcher(command, &a)
    }

    #[test]
    fn unpinned_launchers_are_reported() {
        for (c, a) in [
            ("npx", vec!["-y", "some-mcp"]),
            ("npx", vec!["-y", "@scope/some-mcp"]),
            ("npx", vec!["some-mcp@latest"]),
            ("npx", vec!["some-mcp@^1.2.0"]),
            ("/usr/bin/npx", vec!["some-mcp"]),
            ("bunx", vec!["some-mcp"]),
            ("pnpm", vec!["dlx", "some-mcp"]),
            ("uvx", vec!["some-mcp"]),
            ("uvx", vec!["--from", "x", "some-mcp"]),
            ("pipx", vec!["run", "some-mcp"]),
            ("docker", vec!["run", "-i", "--rm", "ghcr.io/x/mcp"]),
            ("docker", vec!["run", "-i", "--rm", "ghcr.io/x/mcp:latest"]),
            ("podman", vec!["run", "-e", "K=V", "img"]),
            ("npx.cmd", vec!["some-mcp"]),
        ] {
            assert!(w(c, &a).is_some(), "not reported: {c} {a:?}");
        }
    }

    #[test]
    fn pinned_and_other_commands_are_not() {
        for (c, a) in [
            ("npx", vec!["-y", "some-mcp@1.2.3"]),
            ("npx", vec!["-y", "@scope/some-mcp@1.2.3-beta.1"]),
            ("pnpm", vec!["dlx", "some-mcp@2.0.0"]),
            ("uvx", vec!["some-mcp==1.2.3"]),
            ("uvx", vec!["some-mcp@1.2.3"]),
            ("pipx", vec!["run", "some-mcp==1.2.3"]),
            ("docker", vec!["run", "-i", "--rm", "ghcr.io/x/mcp:1.2.3"]),
            ("docker", vec!["run", "ghcr.io/x/mcp@sha256:abcd"]),
            ("docker", vec!["run", "localhost:5000/x/mcp:v2"]),
            ("docker", vec!["ps"]),
            ("./server", vec!["--stdio"]),
            ("python3", vec!["server.py"]),
            ("node", vec!["server.js"]),
            ("npx", vec![]),
        ] {
            assert!(
                w(c, &a).is_none(),
                "wrongly reported: {c} {a:?}: {:?}",
                w(c, &a)
            );
        }
    }

    #[test]
    fn an_unknown_docker_flag_is_not_guessed_at() {
        assert!(w("docker", &["run", "--weird", "img"]).is_none());
        assert!(w("docker", &["run", "--rm", "-v", "/a:/b", "img"]).is_some());
    }
}
