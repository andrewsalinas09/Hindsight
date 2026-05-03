# Scope control

This document specifies how users tell Hindsight what to record. Scope control is the most important user-facing aspect of the recorder, because it determines whether the tool is usable on real programs. A debugger that's perfect in theory but produces 50GB traces on a 10-second run is not a debugger anyone uses. The whole product hinges on the user being able to express "record this, but not that" with low effort.

## What we're solving

The user has some code. Most of it they trust. A small region of it they're suspicious of. They want full omniscience inside the suspicious region and normal execution speed outside it. The challenge is that "inside" and "outside" aren't always clean function boundaries. Sometimes the suspicious region is a function but not the math kernel it calls. Sometimes it's a function except for the part inside a loop. Sometimes it's a function but skip every call to a particular library.

We need to give the user a vocabulary that covers the natural cases without requiring them to understand the recorder's internals.

## Levels of granularity

There are six levels of granularity that v0 needs to support, plus a depth parameter that controls how far recording recurses into callees. All of these are in v0 because anything missing creates a class of problem the user can't solve, which means they bounce off the tool and don't come back.

The levels are:

1. **Whole program.** Record everything from start to end. Useful for short programs, single-file scripts, and tests where you want full visibility without thinking about scope.

2. **Module or file.** Record only events in a particular file or module. Useful when the user knows the bug is in a specific source file and doesn't want noise from elsewhere.

3. **Function with depth control.** Record a particular function, and optionally some number of levels of its callees. This is the most common case in practice and the depth parameter is what makes it flexible. `depth=0` records the function itself with no recursion into callees. `depth=1` records the function plus everything it directly calls. `depth=None` (the default) records the function and follows the call tree wherever it goes. Most users will use the default; the others are there for cases where the user wants to limit blast radius.

4. **Recording with exclusions.** Any of the above scopes, with patterns that suspend recording when execution enters certain functions or libraries. Handles "this function calls into numpy and I don't want to record numpy."

5. **Region within a function.** Record a function but skip a specific block of code inside it. Handles "this function has a billion-iteration loop in the middle that I don't care about, but I do care about everything else in the function."

6. **Conditional capture.** Record only when some predicate is true. "Only record this function when the input has more than 1000 elements," or "only record when this flag is set." Useful for debugging issues that only manifest in specific cases without paying the recording cost on the common path.

## The user-facing API

The API is layered. The simple cases use a single decorator with no arguments and no thought; the complex cases compose. The progression from simple to complex is gradual, with no cliff.

### Layer 1: the decorator and context manager

The entry point. Decorate a function and it gets recorded along with everything it calls.

```python
@hindsight.record
def process_request(req):
    ...
```

Or, equivalently, wrap a block:

```python
def main():
    with hindsight.record():
        process_request(req)
```

These two forms are interchangeable. The decorator is sugar for the context manager applied to the function body. By default, this records the decorated/wrapped scope and everything it transitively calls — equivalent to `depth=None`.

To limit how far recording recurses into callees, pass a `depth` argument:

```python
@hindsight.record(depth=0)   # this function fully, no callees
def process_request(req):
    ...

@hindsight.record(depth=1)   # this function plus its direct callees
def process_request(req):
    ...

@hindsight.record(depth=None)  # this function and all transitive callees (default)
def process_request(req):
    ...
```

Depth controls recursion into nested function calls, not the granularity of recording within a recorded scope. **A function that's in scope is always fully recorded** — every line, every variable, every branch, every call event. Depth only governs whether we follow into the callees' interiors. So `depth=0` records the decorated function completely, including all its internal logic, but treats every function it calls as a black box: the call site sees the arguments and return value, but the callee's body is not captured. `depth=1` adds one level of recursion, capturing the immediate callees' interiors as well. `depth=None` follows the call tree wherever it goes.

Depth is counted from the recorded scope. A function call from inside a recorded scope increases depth by one; returning decreases it. When depth would exceed the configured limit, the about-to-be-called function is treated as a black box for that call. Excluded functions don't count against the depth budget — they're skipped entirely, not consumed.

### Layer 2: include and exclude patterns

Both forms accept arguments that describe what to skip or include within the recorded scope. Patterns are glob-matched against module names, qualified function names, and file paths.

```python
@hindsight.record(exclude=["numpy.*", "scipy.*", "*.tight_loop"])
def process_request(req):
    ...
```

This says: record everything inside `process_request` and its callees, but if execution enters a function in numpy, scipy, or any function with a qualified name ending in `.tight_loop`, suspend recording until execution returns from that function.

The same form supports an `include` argument that whitelists rather than blacklists:

```python
@hindsight.record(include=["myapp.*"])
def main():
    ...
```

This records only functions matching the pattern, skipping everything else (including the standard library, third-party packages, and any other code paths). Useful when the user wants tight focus on their own code.

`include` and `exclude` can be combined. The semantics are: a function is recorded if it matches the include patterns (or if there are no include patterns, meaning everything is included by default) and does not match the exclude patterns.

### Layer 3: inline skip blocks

Sometimes the region to exclude is inside a function, not a separate function. For that, expose a context manager that suspends recording temporarily within a recorded scope:

```python
@hindsight.record
def process_request(req):
    parsed = parse(req)
    
    with hindsight.skip():
        # Heavy work we don't need to trace.
        for i in range(1_000_000_000):
            heavy_math(i)
    
    result = build_response(parsed, computed_state)
    return result
```

Inside the `skip` block, recording is suspended. The program runs at full speed. When the block exits, recording resumes and captures the rest of the function.

Skip blocks nest correctly. A skip inside a skip is a no-op (already not recording); a record inside a skip resumes recording for the duration of that inner block.

### Layer 4: the configuration file

Per-call arguments get tedious for users who have consistent rules across their project. A `hindsight.toml` at the project root defines global include and exclude patterns:

```toml
[recording]
exclude = [
    "defaults",
    "myapp.migrations.*",
    "myapp.legacy.*",
]

include = []  # empty means "include everything not excluded"
```

The `defaults` token works the same way in the config file as it does in per-call arguments: it expands to the default exclusion list shipped with Hindsight. Users who want to start from scratch can simply omit it. Users who want to extend the defaults list it explicitly alongside their additions, as shown above.

The decorator and context manager merge config-file rules with anything specified at the call site. The user writes `@hindsight.record` without arguments and gets the config-file behavior; per-call arguments take precedence over the config file when they conflict — the user's explicit intent at the call site wins.

### Layer 5: conditional recording

For the case where the user wants to record only under certain conditions, the decorator and context manager accept a predicate:

```python
@hindsight.record(when=lambda req: len(req.items) > 1000)
def process_large_request(req):
    ...
```

The predicate is evaluated at function entry. If it returns truthy, recording proceeds for that invocation. If falsy, the function runs without recording. Each invocation is evaluated independently, so a function might be recorded for some calls and not others depending on its arguments.

Predicates have access to the function's arguments. They should be cheap to evaluate, since they run on every call.

The context-manager form takes a boolean rather than a callable, since there's no per-invocation context:

```python
def main():
    with hindsight.record(when=os.environ.get("DEBUG_MODE")):
        process_request(req)
```

### Whole-program recording

For levels 1 and 2 (whole program and module-scoped), use the CLI rather than source-level decorators:

```bash
hindsight record python myscript.py
```

records the whole program. To scope to specific modules:

```bash
hindsight record --include "myapp.*" --exclude "myapp.utils.*" python myscript.py
```

This launches Python with the recorder pre-configured. No source modifications needed. The CLI flags accept the same patterns as the decorator's arguments.

## How exclusion behaves at runtime

When a recorded function calls an excluded function, two things happen:

1. The call itself is recorded as a `function_call` event with the callee's name, arguments, return value, and duration. This is captured at the call site, which is in a recorded scope.
2. The interior of the excluded function is not recorded. No line events, no branch events, no nested call events from inside the excluded function.

This is the right semantic because it preserves what the LLM needs to reason: it can see that the function was called, what it was called with, and what it returned. It cannot see the function's internals. If the LLM needs the internals, it can read the source of the excluded function and reason about it; the trace plus the source is enough for most debugging questions.

The trace format includes a flag on `function_call` events indicating whether the call was scoped out, so the LLM can correctly distinguish "this function was excluded by the recorder" from "this function had no interesting internal events." When the LLM responds to a user question about an excluded call, it can say something like "I don't have internals for this call because it was excluded; if you want to see inside, run again with the exclusion removed."

This contrasts with the `skip` block, which says don't record anything about this region, including any function calls inside it. Skip is a stronger statement: the user is telling us they want a black box, not just to skip recursion. Both are useful and they correspond to different user intents, so both are exposed.

## Default exclusions

A new user who decorates a function and runs their program should get useful output without first having to learn the exclusion system. To make that happen, Hindsight ships with a default exclusion list applied automatically unless the user overrides it. The list includes:

- Standard library modules that are commonly called many times and rarely interesting (`logging`, `re`, `json`, `os.path`, `pathlib`, `collections`, parts of `typing`)
- Heavy third-party libraries that are well-tested and produce noisy traces (`numpy`, `pandas`, `scipy`, `torch`, `tensorflow`, `requests`, `urllib3`, `sqlalchemy`)
- Test framework internals (`pytest`, `unittest`, `_pytest`)
- Common dependencies of the above

The default list is conservative — it excludes things almost everyone wants excluded.

The defaults are referenced by the special token `defaults` in any exclude list. This makes the relationship explicit:

- `exclude=[]` means no exclusions at all, including dropping the defaults. Record everything.
- `exclude=[defaults]` means use only the default exclusions. Same as not specifying `exclude` at all.
- `exclude=[defaults, "myapp.helpers.*"]` means use the defaults plus an additional pattern.
- `exclude=["myapp.helpers.*"]` means use only the user's pattern, dropping the defaults entirely.

The token form removes the ambiguity of "does an empty list mean nothing-excluded or just-the-defaults." The user states their intent explicitly. Most users will write `exclude=[defaults, ...]` when they want to extend the defaults, which is the common case.

The default list is shipped as a TOML file inside the package and is the source of truth. It will evolve as we learn what users actually want. Users can see what's currently in it via `hindsight defaults --show`.

## The recording metadata tool

The most likely failure mode in scope control is the user getting a result they don't expect and not understanding why. To make this debuggable, the recorder writes its scope decisions into the trace as metadata, and the MCP server exposes a tool that reports them.

The metadata captures:

- The full include and exclude pattern sets in effect (merged from defaults, config file, and per-call arguments)
- The list of code objects that were recorded
- The list of code objects that were seen but excluded, with which pattern matched each one
- The list of skip blocks encountered during recording

When the user asks "what did we actually record" or "why isn't there data for this function," the LLM can call the tool and get a precise answer. This turns scope-related confusion from a frustration into a five-second clarification.

## What's not in v0

The six granularity levels above, with depth control on the function-scoped case, are all in v0. A few things adjacent to scope control are explicitly not:

- **Sampling.** "Record 1 in 100 invocations of this function." Useful for production-style debugging but not necessary for the development-time use cases v0 targets.
- **Time-budget-based recording.** "Record up to 30 seconds of execution, then stop." Solves a different problem (long-running programs) and adds complexity to the recorder's lifecycle.
- **Sub-line region scoping.** Recording a specific expression within a line. Probably never necessary; if it is, the user can refactor the expression into a function and scope that.
- **Recording across thread or process boundaries.** Single-process, single-interpreter only in v0. Multi-process and async-aware tracing is a larger architectural change for a later version.

These are all reasonable additions later. They should not gate v0.

## How this connects to the rest of the system

The scope control system produces decisions about which code objects to record. Those decisions are passed to the `sys.monitoring` API at registration time, which is what makes the recorder fast — the interpreter does the filtering at the C level and never invokes our handlers for excluded code. This is why scope control can be aggressive without performance penalty: the cost we pay is per-recorded-function, not per-program-function.

The trace format records scope decisions as metadata in the file header, plus the per-call exclusion flags described above. The indexer loads this metadata into the database alongside the events. The MCP server exposes it via the recording-metadata tool.

The user never has to touch any of this. They write a decorator or a config file; everything else happens automatically.

## Worked examples

Some examples of how the layers compose, since the API has more surface than any single example shows.

**The simplest case:**

```python
@hindsight.record
def buggy():
    ...
```

Records `buggy` and everything it calls, with default exclusions applied.

**Tight focus:**

```python
@hindsight.record(include=["myapp.*"], exclude=[])
def buggy():
    ...
```

Records only functions in `myapp`, with no exclusions at all (including dropping the default exclusions). Anything outside `myapp` is silently skipped because of the include filter.

**Surgical exclusion:**

```python
@hindsight.record(exclude=[defaults, "myapp.expensive_helper"])
def buggy():
    ...
```

Records `buggy` and its callees with the default exclusions plus one specific helper that the user knows is expensive and uninteresting.

**Limiting recursion depth:**

```python
@hindsight.record(depth=1)
def buggy():
    ...
```

Records `buggy` and the functions it directly calls, but doesn't recurse further. Useful when the user wants to see what the immediate callees did without drowning in the deeper call tree.

**Inline skip:**

```python
@hindsight.record
def buggy():
    setup()
    with hindsight.skip():
        run_simulation()
    teardown()
```

Records `setup` and `teardown` and their callees. Records that `run_simulation` was reached but doesn't record into it or anything it calls.

**Conditional recording:**

```python
@hindsight.record(when=lambda *args, **kwargs: should_debug())
def maybe_buggy():
    ...
```

Records the function only when `should_debug()` returns true. The function still runs every time, but only some invocations produce trace data.

**Whole program from CLI:**

```bash
hindsight record python script.py
```

No decorators needed. Records the entire execution with default exclusions.

**Whole program with custom scope:**

```bash
hindsight record --include "myapp.*" python script.py
```

Records only functions matching the pattern across the whole program execution.

These six patterns cover essentially every real debugging scenario.

## Summary

Scope control in v0 supports six granularity levels with depth control for function-scoped recording, expressed through a layered API: decorator and context manager for the basic case, depth parameter for limiting recursion, include/exclude patterns for filtering, skip blocks for inline exclusion, a config file for project-wide rules, conditional predicates for selective recording, and CLI flags for whole-program scope. Default exclusions ship with the tool and are referenced by the explicit `defaults` token in user exclude lists, so users can extend or replace them unambiguously. A metadata tool exposes scope decisions to the LLM so users can debug their scoping when it surprises them.

The design principle is that simple cases use the simple API and complex cases compose smaller pieces, with no cliff between them. Users should not have to read this document to use the tool — they should be able to write `@hindsight.record` and have something useful happen, then learn the rest as their needs grow.
