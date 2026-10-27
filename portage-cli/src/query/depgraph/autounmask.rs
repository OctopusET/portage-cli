use std::io::Write as _;

use camino::Utf8Path;

use super::repo::AutounmaskCandidate;
use super::repo::FilterReason;

/// One line to write into a portage config file
struct Entry {
    filename: String,
    atom: String,
    /// Trailing tokens after the atom (keywords, licenses); empty for unmask
    tokens: Vec<String>,
}

fn filename(cpv: &portage_atom::Cpv) -> String {
    format!("{}-{}", cpv.cpn.category.as_str(), cpv.cpn.package.as_str())
}

fn atom(
    cpv: &portage_atom::Cpv,
    slot: Option<portage_atom::interner::Interned<portage_atom::interner::DefaultInterner>>,
) -> String {
    let slot_suffix = slot.map(|s| format!(":{}", s.as_str())).unwrap_or_default();
    format!("={}-{}{}", cpv.cpn, cpv.version, slot_suffix)
}

/// Slot-scoped form for widened selections: `cat/pkg:SLOT` — survives
/// point-release churn where an exact pin would go stale on the next
/// snapshot. When the slot has live versions and the pick isn't one of them,
/// the grant is bounded below the lowest live version (`<cat/pkg-live:SLOT`)
/// so accepting the slot never invites a `.9999` pick on a later resolve.
fn slot_scoped_atom(
    cpv: &portage_atom::Cpv,
    slot: Option<portage_atom::interner::Interned<portage_atom::interner::DefaultInterner>>,
    live_upper_bound: Option<&portage_atom::Version>,
) -> String {
    let Some(s) = slot else {
        return cpv.cpn.to_string();
    };
    match live_upper_bound {
        Some(bound) => format!("<{}-{bound}:{}", cpv.cpn, s.as_str()),
        None => format!("{}:{}", cpv.cpn, s.as_str()),
    }
}

fn build_entries(candidates: &[AutounmaskCandidate], kind: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for c in candidates {
        let a = if c.widened && !c.is_live {
            slot_scoped_atom(&c.cpv, c.slot, c.live_upper_bound.as_ref())
        } else {
            atom(&c.cpv, c.slot)
        };
        let f = filename(&c.cpv);
        for reason in &c.reasons {
            match (kind, reason) {
                ("keywords", FilterReason::Keyword(kw)) => {
                    entries.push(Entry {
                        filename: f.clone(),
                        atom: a.clone(),
                        tokens: vec![kw.clone()],
                    });
                }
                ("unmask", FilterReason::Masked) => {
                    entries.push(Entry {
                        filename: f.clone(),
                        atom: a.clone(),
                        tokens: vec![],
                    });
                }
                ("license", FilterReason::License(lics)) => {
                    entries.push(Entry {
                        filename: f.clone(),
                        atom: a.clone(),
                        tokens: lics.clone(),
                    });
                }
                ("properties", FilterReason::Properties(props)) => {
                    entries.push(Entry {
                        filename: f.clone(),
                        atom: a.clone(),
                        tokens: props.clone(),
                    });
                }
                ("restrict", FilterReason::Restrict(r)) => {
                    entries.push(Entry {
                        filename: f.clone(),
                        atom: a.clone(),
                        tokens: r.clone(),
                    });
                }
                _ => {}
            }
        }
    }
    // Deduplicate by (filename, atom, tokens).
    entries.sort_by(|a, b| (&a.filename, &a.atom).cmp(&(&b.filename, &b.atom)));
    entries.dedup_by(|a, b| a.filename == b.filename && a.atom == b.atom && a.tokens == b.tokens);
    entries
}

fn format_line(e: &Entry) -> String {
    if e.tokens.is_empty() {
        e.atom.clone()
    } else {
        format!("{} {}", e.atom, e.tokens.join(" "))
    }
}

/// How [`report`] words its headlines
pub(super) enum Headline<'a> {
    /// Dropped deps: the plan is incomplete until the user acts
    Required,
    /// Widened selections the solve applied but nothing persists
    InMemory,
    /// Widened selections written under this config dir
    Written(&'a Utf8Path),
}

fn headline(what: &str, page: &str, mode: &Headline) -> String {
    use super::output::C_PKG;

    match mode {
        Headline::Required => {
            format!("{C_PKG}The following {what} are necessary to proceed:{C_PKG:#}")
        }
        Headline::InMemory => format!(
            "{C_PKG}The following {what} were applied in memory to resolve \
             this plan, --autounmask-write persists them:{C_PKG:#}"
        ),
        Headline::Written(dir) => format!(
            "{C_PKG}The following {what} were written to {} to resolve this plan:{C_PKG:#}",
            dir.join(page)
        ),
    }
}

/// Print autounmask changes to stderr in portage style
/// Order: mask → keywords → license (most fundamental first).
pub(super) fn report(candidates: &[AutounmaskCandidate], mode: Headline) {
    use super::output::{C_DIM, C_ON, C_PKG};

    let unmask = build_entries(candidates, "unmask");
    let kw = build_entries(candidates, "keywords");
    let lic = build_entries(candidates, "license");
    let props = build_entries(candidates, "properties");
    let restr = build_entries(candidates, "restrict");

    if unmask.is_empty() && kw.is_empty() && lic.is_empty() && props.is_empty() && restr.is_empty()
    {
        return;
    }

    let mut out = anstream::stderr();
    let head = |what: &str, page: &str| {
        format!(
            "\n{}\n (see \"{page}\" in the portage(5) man page for more details)",
            headline(what, page, &mode)
        )
    };

    if !unmask.is_empty() {
        writeln!(out, "{}", head("mask changes", "package.unmask")).ok();
        for e in &unmask {
            writeln!(out, "{C_PKG}{}{C_PKG:#}", e.atom).ok();
        }
    }
    if !kw.is_empty() {
        writeln!(
            out,
            "{}",
            head("keyword changes", "package.accept_keywords")
        )
        .ok();
        for e in &kw {
            let token_str = e
                .tokens
                .iter()
                .map(|t| format!("{C_ON}{t}{C_ON:#}"))
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(out, "{C_PKG}{}{C_PKG:#} {token_str}", e.atom).ok();
        }
    }
    if !lic.is_empty() {
        writeln!(out, "{}", head("license changes", "package.license")).ok();
        for e in &lic {
            let token_str = e
                .tokens
                .iter()
                .map(|t| format!("{C_DIM}{t}{C_DIM:#}"))
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(out, "{C_PKG}{}{C_PKG:#} {token_str}", e.atom).ok();
        }
    }
    if !props.is_empty() {
        writeln!(out, "{}", head("property changes", "package.properties")).ok();
        for e in &props {
            let token_str = e
                .tokens
                .iter()
                .map(|t| format!("{C_DIM}{t}{C_DIM:#}"))
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(out, "{C_PKG}{}{C_PKG:#} {token_str}", e.atom).ok();
        }
    }
    if !restr.is_empty() {
        writeln!(
            out,
            "{}",
            head("restrict changes", "package.accept_restrict")
        )
        .ok();
        for e in &restr {
            let token_str = e
                .tokens
                .iter()
                .map(|t| format!("{C_DIM}{t}{C_DIM:#}"))
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(out, "{C_PKG}{}{C_PKG:#} {token_str}", e.atom).ok();
        }
    }
}

/// Write autounmask entries to the appropriate files under `portage_dir`
/// (`portage_dir` is e.g. `/etc/portage`).
pub(super) fn write(
    candidates: &[AutounmaskCandidate],
    portage_dir: &Utf8Path,
) -> anyhow::Result<()> {
    write_kind(
        candidates,
        "keywords",
        &portage_dir.join("package.accept_keywords"),
    )?;
    write_kind(candidates, "unmask", &portage_dir.join("package.unmask"))?;
    write_kind(candidates, "license", &portage_dir.join("package.license"))?;
    write_kind(
        candidates,
        "properties",
        &portage_dir.join("package.properties"),
    )?;
    write_kind(
        candidates,
        "restrict",
        &portage_dir.join("package.accept_restrict"),
    )?;
    Ok(())
}

fn write_kind(
    candidates: &[AutounmaskCandidate],
    kind: &str,
    dir: &Utf8Path,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let entries = build_entries(candidates, kind);
    if entries.is_empty() {
        return Ok(());
    }

    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {dir}"))?;

    // Group by filename.
    let mut by_file: std::collections::HashMap<&str, Vec<&Entry>> =
        std::collections::HashMap::new();
    for e in &entries {
        by_file.entry(e.filename.as_str()).or_default().push(e);
    }

    for (filename, lines) in by_file {
        let path = dir.join(filename);
        let existing = if path.exists() {
            std::fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?
        } else {
            String::new()
        };
        let new_content = merge_content(&existing, lines);
        std::fs::write(&path, &new_content).with_context(|| format!("failed to write {path}"))?;
        tracing::info!("wrote {path}");
    }
    Ok(())
}

fn merge_content(existing: &str, lines: Vec<&Entry>) -> String {
    let mut output: Vec<String> = existing.lines().map(str::to_string).collect();
    while output
        .last()
        .map(|l: &String| l.trim().is_empty())
        .unwrap_or(false)
    {
        output.pop();
    }
    for entry in lines {
        let new_line = format_line(entry);
        let existing_pos = output
            .iter()
            .position(|l| l.split_whitespace().next() == Some(entry.atom.as_str()));
        if let Some(pos) = existing_pos {
            output[pos] = new_line;
        } else {
            if !output.is_empty() {
                output.push(String::new());
            }
            output.push(new_line);
        }
    }
    let mut result = output.join("\n");
    result.push('\n');
    result
}

#[cfg(test)]
mod tests {
    use portage_atom::interner::Interned;

    use super::*;

    fn candidate(is_live: bool) -> AutounmaskCandidate {
        let ver = if is_live { "9999" } else { "1.0" };
        AutounmaskCandidate {
            cpv: portage_atom::Cpv::parse(&format!("dev-libs/foo-{ver}")).unwrap(),
            slot: Some(Interned::intern("0")),
            reasons: vec![FilterReason::Masked],
            widened: true,
            live_upper_bound: None,
            is_live,
        }
    }

    #[test]
    fn widened_live_selection_persists_as_exact_pin() {
        let entries = build_entries(&[candidate(true)], "unmask");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].atom, "=dev-libs/foo-9999:0");
    }

    #[test]
    fn widened_non_live_selection_persists_slot_scoped() {
        let entries = build_entries(&[candidate(false)], "unmask");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].atom, "dev-libs/foo:0");
    }

    #[test]
    fn written_headline_names_the_file() {
        let dir = Utf8Path::new("/etc/portage");
        let h = headline("mask changes", "package.unmask", &Headline::Written(dir));
        assert!(
            h.contains("were written to /etc/portage/package.unmask"),
            "{h}"
        );
        assert!(!h.contains("in memory"), "{h}");
    }
}
