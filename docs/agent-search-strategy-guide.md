# Agent Search Strategy Guide

Instructions for an AI agent on how to use `colgrep`, `rg` (ripgrep), and `glob` to efficiently search a large codebase. The goal is to minimize scope quickly and reach the right files in the fewest steps.

---

## Available Tools

| Tool | What it does | Speed | Best for |
|------|-------------|-------|----------|
| `glob` | Match files by name/path pattern | Instant | Finding files by name, extension, directory |
| `rg` (ripgrep) | Exact text/regex search in file contents | <1s | Known identifiers, strings, imports, references |
| `colgrep` (daemon) | Semantic + keyword code search via ColBERT index | 30ms-2s | Exploration, conceptual queries, unknown code |

### ColGREP Search Modes

| Mode | Alpha | When to use |
|------|-------|-------------|
| **Hybrid** (default) | `0.75` | General purpose — combines semantic understanding with keyword matching |
| **Semantic** | `1.0` | Pure natural language — "how does error handling work in the auth system" |
| **BM25 keyword** | `0.0` | You know partial keywords but not exact identifiers |

---

## How to Call Each Tool

**glob / rg**: Called directly as CLI tools (standard agent tools).

**colgrep**: Called via HTTP API to the daemon running at `http://colgrep-server:8080`. Do NOT spawn `colgrep search` as a CLI process — the daemon keeps the model and index in memory for fast responses.

```
# Correct — call the daemon API
POST http://colgrep-server:8080/search
{"query": "thermal sensor duty cycle", "top_k": 10, "alpha": 0.75}

# Wrong — do NOT do this (spawns fresh process, reloads model, 20-30s)
colgrep search "thermal sensor duty cycle" /path/to/repo
```

---

## Decision Tree

```
User asks a question about the codebase
  |
  ├─ Do you know the EXACT symbol name, string, or identifier?
  │    YES → use `rg` (ripgrep)
  │    Example: "find all uses of DatabaseManager"
  │    → rg "DatabaseManager" --type cs
  │
  ├─ Do you know the FILE NAME or pattern?
  │    YES → use `glob`
  │    Example: "find all controller files"
  │    → glob "**/Controller*.cs" or glob "**/*controller*"
  │
  ├─ Do you know KEYWORDS but not exact names?
  │    YES → use `colgrep` with hybrid (alpha=0.75)
  │    Example: "find code related to thermal sensor duty cycle"
  │    → POST /search {"query": "thermal sensor duty cycle", "alpha": 0.75}
  │
  ├─ Do you only have a CONCEPT or DESCRIPTION?
  │    YES → use `colgrep` with semantic (alpha=1.0)
  │    Example: "how does the application handle authentication"
  │    → POST /search {"query": "authentication flow login session", "alpha": 1.0}
  │
  └─ Unsure / broad exploration?
       → Start with colgrep hybrid, then narrow with rg
```

---

## Patterns

### Pattern 1: Explore → Confirm

Use when the user asks about functionality you haven't seen before.

```
Step 1: colgrep hybrid search to find the right area (1-2 queries max)
Step 2: rg to find exact references and usage
Step 3: Read the files to understand
```

**Example:** User asks "how does the reagent cooling system work?"

```
1. POST /search {"query": "reagent cooling temperature control", "alpha": 0.75, "top_k": 5}
   → Returns: ThermalViewModel.cs, ReagentServer.cs, CoolingLoop.cs

2. rg "ReagentServer" --type cs  (confirm exact class name and find all references)
   → Returns: 12 files referencing ReagentServer

3. Read ThermalViewModel.cs and ReagentServer.cs
```

**Why not just rg?** You don't know the symbol name "ReagentServer" until colgrep finds it. Guessing keywords with rg would take 5-10 attempts.

### Pattern 2: Direct Lookup

Use when the user mentions a specific symbol, file, or error message.

```
Step 1: rg for the exact term
Step 2: Read the matching files
```

**Example:** User asks "where is ThermalsControl defined?"

```
1. rg "class ThermalsControl" --type cs
   → Returns: ThermalsControl.cs:15

2. Read ThermalsControl.cs
```

**Why not colgrep?** You already know the exact name. rg is instant and precise.

### Pattern 3: File Discovery

Use when the user asks about a category of files or structure.

```
Step 1: glob to find files matching a pattern
Step 2: rg within those files if needed
```

**Example:** User asks "what test files exist for the auth module?"

```
1. glob "**/auth/**/*test*" or glob "**/auth/**/*Test*"
   → Returns: AuthServiceTests.cs, AuthMiddlewareTest.cs, ...

2. Read the files
```

### Pattern 4: Scoped Semantic Search

Use when you know the area but not the specifics.

```
Step 1: colgrep with target_paths to scope to a directory
Step 2: rg to confirm findings
```

**Example:** User asks "how does error handling work in the networking layer?"

```
1. POST /search {"query": "error handling retry timeout", "alpha": 0.75, "target_paths": ["src/networking/"]}
   → Returns: NetworkRetryPolicy.cs, ConnectionErrorHandler.cs

2. rg "catch|ErrorHandler|RetryPolicy" src/networking/ --type cs
   → Confirms all references
```

### Pattern 5: Fallback Chain

Use when the first tool returns nothing useful.

```
POST /search {"query": "...", "alpha": 0.75} → 0 results?
  → Try BM25: POST /search {"query": "...", "alpha": 0.0}
    → Still 0? Try rg with guessed keywords
      → Still 0? Try glob for file names
        → Still 0? Ask the user for more context
```

---

## Rules

### DO

1. **Use colgrep surgically** — 1-2 queries to orient, then switch to rg for precision
2. **Always scope when possible** — use `target_paths` to limit colgrep to a directory
3. **Use hybrid mode (alpha=0.75) as default** — it combines semantic understanding with keyword matching
4. **Switch to rg immediately** once you know the symbol name from colgrep results
5. **Use glob first** when the question is about file structure, not code content
6. **Read the colgrep results** — they include file, line, signature, and code snippet. Often you don't need a follow-up rg query

### DON'T

1. **Don't run colgrep 10 times** — if 2 queries return nothing useful, switch to rg
2. **Don't use colgrep for exact symbol lookup** — rg is 100x faster for "find class Foo"
3. **Don't use rg for conceptual questions** — "how does auth work" can't be answered by grepping a keyword
4. **Don't use BM25 (alpha=0.0) as default** — it misranks results (picks README files, resource files). Use hybrid
5. **Don't run repo-wide colgrep when you can scope** — `target_paths` reduces latency and improves relevance
6. **Don't retry the same failed colgrep query rephrased 5 times** — after 2 failures, switch tools

---

## ColGREP API Reference

### Search Request

```json
POST /search
{
  "query": "natural language description of what you're looking for",
  "top_k": 10,
  "alpha": 0.75,
  "target_paths": ["src/modules/auth/"],
  "include_patterns": ["*.cs"],
  "exclude_dirs": ["test", "vendor"],
  "code_only": true
}
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `query` | required | Natural language or keyword query |
| `top_k` | 10 | Number of results to return |
| `alpha` | 0.75 | 0.0=BM25, 0.75=hybrid, 1.0=semantic |
| `target_paths` | all | Restrict to specific directories |
| `include_patterns` | all | Glob patterns for file types (e.g. `["*.cs", "*.xaml"]`) |
| `exclude_dirs` | none | Directories to skip |
| `code_only` | false | Exclude raw code blocks, only return functions/classes/methods |

### Search Response

```json
{
  "results": [
    {
      "file": "src/auth/AuthService.cs",
      "path": "src/auth/AuthService.cs",
      "line": 42,
      "start_line": 42,
      "end_line": 78,
      "score": 0.016,
      "unit_type": "class",
      "signature": "public class AuthService : IAuthService",
      "code": "public class AuthService...",
      "language": "csharp"
    }
  ],
  "search_time_ms": 35,
  "latency_ms": 35,
  "index_doc_count": 16119,
  "hybrid_mode": true
}
```

### Health Check

```json
GET /health
→ {"status": "ready", "index_doc_count": 16119, "model": "lightonai/LateOn-Code-edge"}
```

---

## Choosing Alpha: Quick Reference

| Situation | Alpha | Why |
|-----------|-------|-----|
| General search, first attempt | **0.75** | Best balance of semantic + keyword |
| "How does X work?" (conceptual) | **1.0** | Need semantic understanding, no keywords to match |
| Know partial keywords | **0.0** | BM25 keyword matching, skip semantic |
| Semantic returned 0 results | **0.0** | Fall back to keyword-only |
| Searching for exact identifier via colgrep | **0.0** | BM25 is better for exact token matches |

---

## Performance Expectations

| Index size | Daemon warm latency | Notes |
|-----------|-------------------|-------|
| 500 files / 2.5k docs | 20-40ms | Small project |
| 4k files / 16k docs | 26-88ms | Medium project (C#) |
| 164k files / 3.5M docs | 1.7-2.2s | Very large project (Chromium-scale) |

The daemon must be running (`colgrep serve`). If the daemon is not available, fall back to ripgrep only.

---

## Example Workflows

### "Explain how feature X works"

```
1. POST /search {"query": "feature X implementation", "alpha": 0.75, "top_k": 5}
2. Read the top 2-3 results (signature + code are in the response)
3. If needed: rg "ClassName" to trace dependencies
4. Synthesize answer from the code
```

### "Find and fix bug in Y"

```
1. rg "error message text" or rg "ExceptionType" — find the exact error
2. Read the file, trace the call stack
3. If stuck: POST /search {"query": "Y error handling failure", "alpha": 0.75}
4. Fix the bug
```

### "Add a new feature similar to Z"

```
1. POST /search {"query": "Z implementation", "alpha": 0.75} — find existing implementation
2. glob "**/*Z*" — find all Z-related files (models, views, tests)
3. Read key files to understand the pattern
4. Implement following the same pattern
```

### "What files would be affected if I change W?"

```
1. rg "class W " or rg "interface W " — find the definition
2. rg "W\b" --type cs — find all references
3. POST /search {"query": "W usage integration", "alpha": 0.75, "target_paths": ["src/"]}
4. Combine results for impact analysis
```
