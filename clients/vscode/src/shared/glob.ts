/**
 * Minimal glob matcher for the `PathPattern` semantics described in
 * SPEC.md 2.4: `**` absorbs any number of path segments, `*` absorbs one
 * segment's characters, everything else must match literally. Paths are
 * matched as POSIX-style, `/`-separated, workspace-relative strings.
 *
 * This intentionally does not attempt the conservative subset-containment
 * algorithm from SPEC.md 2.4 (`is_subset_of`, used for authority
 * attenuation) — the editor surface only needs point membership
 * (`matches`), not containment.
 */

function toRegExp(pattern: string): RegExp {
  let re = "^";
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i];
    if (c === "*" && pattern[i + 1] === "*") {
      // `**` absorbs any segments, including the separators around it.
      re += ".*";
      i++;
      // Swallow an immediately following slash so `a/**/b` matches `a/b`.
      if (pattern[i + 1] === "/") {
        i++;
      }
    } else if (c === "*") {
      re += "[^/]*";
    } else if (c === "?") {
      re += "[^/]";
    } else if (".+^${}()|[]\\".includes(c)) {
      re += "\\" + c;
    } else {
      re += c;
    }
  }
  re += "$";
  return new RegExp(re);
}

/** Normalize backslashes and strip a leading `./` so callers can pass
 * whatever vscode.Uri.fsPath / workspace-relative form they have. */
export function normalizePath(path: string): string {
  return path.replace(/\\/g, "/").replace(/^\.\//, "");
}

export function matchesPath(pattern: string, path: string): boolean {
  return toRegExp(normalizePath(pattern)).test(normalizePath(path));
}

export function matchesAny(patterns: string[], path: string): boolean {
  return patterns.some((p) => matchesPath(p, path));
}
