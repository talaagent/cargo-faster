# Why is my Rust build slow?

You changed one line, ran `cargo check`, and went to make coffee. When you came back, it was still going. Maybe this has always been true of your project, or maybe it crept up on you over a few months, one dependency and one `async fn` at a time. Either way, you'd like to know why.

This guide is about answering that question. Not with a list of tricks to try at random, but by building a picture of what the compiler is actually doing, how to measure where the time goes, and what you can change once you know. Some of the fixes are one line in a config file. Some are design changes to your code. The hard part is usually knowing which is which, and that is what measurement is for.

We'll start with how `rustc` and Cargo spend their time, then learn the tools for measuring it, then go through the most common causes one by one. Near the end there's a case study from a real workspace, where a `cargo clippy` run went from twelve minutes to two and a half, and the cause was not at all what we expected.

A note on versions: Rust changes quickly, and so do its build tools. Where something depends on a particular version, or only works on a nightly compiler, this guide says so. If you're reading this much later than late 2026, check the details against the current documentation.

## How rustc spends its time

Before we can talk about making a build faster, we need a rough model of what a build is. Let's start with one crate.

When Cargo compiles a crate, it runs `rustc` once on that crate's root file (`src/lib.rs` or `src/main.rs`). Everything reachable from there through `mod` declarations is part of the same compilation. This is the first important idea: **the crate is the unit of compilation**. `rustc` never compiles half a crate. If you change one function in a crate with 100,000 lines, `rustc` runs on the whole crate again. (Incremental compilation, which we'll get to, makes that rerun cheaper, but it is still a rerun of the whole crate.)

Inside that one `rustc` invocation, the work happens in roughly these phases:

1. **Parsing and macro expansion.** The source is parsed into a syntax tree. Then macros are expanded: `macro_rules!` macros, built-in macros like `format!`, and procedural macros like `#[derive(Serialize)]` or `#[tokio::main]`. Expansion can create new items, which can contain more macros, so this repeats until nothing is left to expand. Procedural macros are compiled programs that `rustc` loads and runs, so this phase can include running a lot of other people's code.
2. **Name resolution.** Every path, like `std::collections::HashMap` or `self.client`, is resolved to the thing it refers to.
3. **Lowering to HIR.** The expanded syntax tree is lowered into the High-level Intermediate Representation, a desugared form that's easier for the later phases to work with. `for` loops become `loop` and `match`, `async fn` becomes a function returning an `impl Future`, and so on.
4. **Type checking and trait solving.** Every expression gets a type. Every time your code relies on a trait, like calling `.clone()`, using `?`, or passing a value to a function that requires `T: Send`, the compiler has to prove that the trait is implemented. This proving is called *trait solving*, and it is often far more work than it looks like from the source.
5. **MIR building and borrow checking.** Function bodies are lowered again, into the Mid-level IR, a control-flow graph. The borrow checker runs on MIR. So does the transformation that turns an `async fn` body into a state machine, and the checks that need to know what that state machine contains.
6. **Monomorphization.** Generic functions don't produce machine code until they're used with concrete types. The compiler collects every concrete instantiation that's needed, like `Vec<u8>::push` and `Vec<String>::push`, and each one becomes its own function to compile.
7. **LLVM code generation.** The monomorphized MIR is translated to LLVM IR, split into *codegen units*, optimized (depending on your profile), and turned into object files. LLVM runs codegen units in parallel.
8. **Linking.** For a binary, test, or other final artifact, a linker combines your object files with those of every dependency into one executable.

Phases 1 through 5 are often called the *frontend*. Phases 6 and 7 are the *backend*. Linking happens at the end, and only for things that produce an executable or shared library.

### check versus build

`cargo check` runs the frontend and stops. It writes a `.rmeta` file for each crate, which contains the crate's *metadata*: its types, function signatures, trait impls, and the MIR of generic and inline functions that downstream crates may need. It doesn't run LLVM and doesn't link.

`cargo build` does everything, and writes `.rlib` files (metadata plus object code) for libraries and executables for binaries.

So if `cargo check` is fast and `cargo build` is slow, your time is going to code generation and linking. If `cargo check` itself is slow, your time is going to the frontend: macro expansion, type checking, trait solving, borrow checking. These have completely different fixes. Knowing which one you have is the first thing to find out.

One subtlety: even `cargo check` has to fully build some things. Build scripts (`build.rs`) are programs, so they're compiled and run. Procedural macros are programs too, so they're compiled to native code. And their dependencies, like `syn`, are compiled to native code as well. A `cargo check` of a project that uses `serde_derive` still runs LLVM on `syn`.

### Crates as compilation units, and pipelining

A crate can't start compiling until its dependencies are far enough along. How far is "far enough"? Not all the way. To type-check a crate, `rustc` only needs the *metadata* of its dependencies, not their machine code. So Cargo uses *pipelining*: when `rustc` finishes the frontend of crate `a`, it emits `a.rmeta` and keeps going with codegen. As soon as that `.rmeta` exists, Cargo can start compiling crate `b`, which depends on `a`, while `a`'s codegen is still running.

There are exceptions. A procedural macro crate must be completely built before anything that uses it can start, because `rustc` needs to load and execute it. A build script must be compiled and run before its crate starts. And linking a binary needs the finished `.rlib` of everything it depends on.

### Why the shape of the graph matters

Put those ideas together and you get the central fact about multi-crate build times: **the total time is bounded by the longest chain of dependencies, not by the total amount of work**.

Imagine a workspace where `app` depends on `server`, which depends on `db`, which depends on `models`. Each takes 20 seconds. Even with 64 cores, `app` can't start until `server`'s metadata exists, which can't start until `db`'s does, and so on. The chain takes at least 80 seconds, and 63 of your cores are idle for much of it. Now imagine instead that `app` depends on four independent crates of 20 seconds each. With four free cores, those take 20 seconds in total, and then `app` follows.

The same amount of code, in a different shape, builds in a fraction of the time. We'll come back to this when we talk about the shape of your workspace. First, let's learn how to see the shape of your own build.

## Measure before you change anything

It's tempting to start by trying things: switch the linker, turn off debug info, add a cache. Some of those will help a little. But build time problems are usually dominated by one or two causes, and the causes are often surprising. If you guess, you'll spend your effort on the 5% and miss the 80%.

So we'll measure first. There are three levels of detail:

- **The build timeline** tells you which crates take time and how they're arranged. Start here.
- **Self-profiling** tells you what `rustc` is doing inside one slow crate.
- **Counting LLVM lines** tells you which generic functions are producing the most code.

### The build timeline

Run your usual build with `--timings`:

```console
$ cargo build --timings
   Compiling proc-macro2 v1.0.89
   ...
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 42s
      Timing report saved to /home/you/proj/target/cargo-timings/cargo-timing-20261117T101533.123Z.html
```

`--timings` works with `check`, `clippy`, `test`, and the other build commands, too. Cargo writes an HTML report to `target/cargo-timings/`, with a timestamped copy for each run and `cargo-timing.html` for the latest. Open it in a browser.

The top of the report has a summary: how many units were built, the total time, the maximum concurrency, and the `rustc` version. A *unit* is one `rustc` invocation: a crate's library, its test harness, its binary, its build script, and so on. One crate can be several units.

Below that is the chart. Each horizontal bar is one unit, laid out on a timeline. The bar starts when `rustc` started and ends when it finished. If the unit pipelines, the bar is split into two shades: the first part is the frontend, up to the moment the `.rmeta` was written, and the second part is code generation. Under `cargo check` there's no codegen section, since there's no codegen.

Here are the things to look for in the chart:

- **Long bars.** A single unit that runs for a long time is a candidate for self-profiling.
- **Long bars with nothing beside them.** If the chart narrows to one or two bars for a stretch of time, your build is serialized there. Everything else is waiting on that unit.
- **Long codegen sections.** A unit whose bar is mostly codegen is spending its time in LLVM. That points at generics and monomorphization, optimization levels, or codegen units, not at the frontend.
- **Staircases.** A diagonal of bars, each starting when the previous one ends, is a dependency chain. That chain is your critical path.

The **critical path** is the longest chain of dependent units from the start of the build to the end. Its length is the fastest your build can possibly be with infinite cores. Making a unit that's not on the critical path faster will often not change your wall time at all, because something else was going to be waiting anyway.

Below the chart is a second graph showing concurrency over time: how many units were active, how many were *waiting* (ready to start but with no free job slot), and how many were *inactive* (blocked on dependencies). Lots of waiting means you're CPU-bound, and more cores would help. Lots of inactive with few active means you're bound by the graph's shape, and more cores won't.

At the bottom is a table of every unit with its total time and its codegen time, sorted longest first. For each unit, Cargo also records which other units became ready to start when this one finished: its *unlocked units*. A unit that unlocks a lot of others is a bottleneck. If it finished earlier, a whole set of work could start earlier. In the chart, you can see these relationships drawn between bars. The report distinguishes units unlocked by this unit's metadata (the `.rmeta`, under pipelining) from units unlocked only when it finished entirely, like a binary waiting for an `.rlib`.

`cargo faster timings` reads this report for you and prints the critical path and the codegen-heavy units.

### Self-profiling one crate

The timeline tells you *which* crate is slow. To find out *why*, you need to look inside `rustc`. The compiler has a built-in profiler for this, enabled with the unstable `-Z self-profile` flag. It records how long each of the compiler's internal *queries* took.

Unstable flags normally need a nightly compiler. But you probably want to profile with the same compiler you build with, since the answer can change between versions. You can do that with `RUSTC_BOOTSTRAP=1`, which tells a stable `rustc` to accept unstable flags:

```console
$ CARGO_INCREMENTAL=0 RUSTC_WRAPPER= RUSTC_BOOTSTRAP=1 \
    cargo rustc -p my-app --lib --profile=check -- \
    -Zself-profile=/tmp/prof -Zself-profile-events=default,args
```

Let's go through that command piece by piece.

`cargo rustc -p my-app --lib` compiles one target of one package, and lets you pass extra flags to `rustc` for that crate only, after the `--`. Dependencies are built normally, without the flags. `--profile=check` is special in `cargo rustc`: it means "do what `cargo check` does". Leave it off to profile a full build including codegen.

`-Zself-profile=/tmp/prof` turns on the profiler and writes the results to that directory, in a file named like `my_app-1234567.mm_profdata`.

`-Zself-profile-events=default,args` records the default events plus *arguments*: for each query, the key it was called with. With `args`, you don't just learn that type checking took 40 seconds; you learn which function it was type checking. This makes the profile larger and the compiler a bit slower, but it's what makes attribution possible.

`RUSTC_BOOTSTRAP=1` is fine for measuring. It is the escape hatch the Rust project uses to build the compiler itself. It doesn't change how your code is compiled; it only unlocks the flags. Don't put it in your build configuration to use unstable features in the code you ship, though. Unstable features can change or vanish in any release, and the stability guarantee you rely on when you upgrade Rust no longer covers you.

`CARGO_INCREMENTAL=0` turns off incremental compilation for this run. With incremental on, `rustc` loads many query results from its cache instead of computing them, so the profile would show you the cost of a partial rebuild, not of the work itself. You want to see the work.

`RUSTC_WRAPPER=` (set to empty) disables any compiler wrapper like `sccache`. If a wrapper has this exact compilation cached, it will hand back the cached output and `rustc` never runs, so you get no profile at all.

Because you've changed `rustc`'s flags, Cargo will consider the crate out of date and recompile it, which is what you want.

To read the profile, install the tools from the `measureme` repository:

```console
$ cargo install --git https://github.com/rust-lang/measureme summarize crox flamegraph
```

`summarize` gives you a table of queries:

```console
$ summarize summarize /tmp/prof/my_app-1234567.mm_profdata
+-------------------------------+-----------+-----------------+----------+------------+
| Item                          | Self time | % of total time | Time     | Item count |
+-------------------------------+-----------+-----------------+----------+------------+
| evaluate_obligation           | 32.07s    | 82.880          | 32.32s   | 123475     |
| type_op_prove_predicate       | 1.41s     | 3.638           | 1.53s    | 18197      |
| check_coroutine_obligations   | 1.33s     | 3.446           | 31.43s   | 513        |
| mir_borrowck                  | 548.84ms  | 1.418           | 34.08s   | 513        |
| typeck_root                   | 412.29ms  | 1.066           | 2.07s    | 536        |
| layout_of                     | 185.49ms  | 0.479           | 16.11s   | 76829      |
| expand_proc_macro             | 75.86ms   | 0.196           | 75.86ms  | 50         |
| ...                           |           |                 |          |            |
+-------------------------------+-----------+-----------------+----------+------------+
```

This is a real profile (trimmed to fewer rows and columns), from the case study at the end of this guide. The two time columns matter most:

- **Self time** is time spent in the query itself, not counting other queries it called.
- **Time** (total time) includes everything it called.

Queries nest. `mir_borrowck` calls `check_coroutine_obligations`, which calls `evaluate_obligation` thousands of times. So `mir_borrowck` has a small self time (0.5 s) and a huge total time (34 s), while `evaluate_obligation` has a huge self time. Read the table like this: *self time tells you where the work is; total time tells you who asked for it*. Here, almost all the work is `evaluate_obligation`, and it's being asked for by borrow checking and coroutine checks, which is to say, by `async` code.

Some query names you'll see often, and what they mean:

- `expand_crate`, `expand_proc_macro`: macro expansion. `expand_proc_macro` is time spent running procedural macros.
- `typeck` (`typeck_root` in recent versions): type checking a function body.
- `evaluate_obligation`: trait solving. One *obligation* is one thing to prove, like `Vec<String>: Clone` or `MyFuture: Send`.
- `type_op_prove_predicate`: also proving trait bounds, on behalf of the borrow checker.
- `mir_borrowck`: borrow checking one function body.
- `check_coroutine_obligations`: checking the deferred obligations of a coroutine, which is what an `async` block or `async fn` body becomes. This is where "is this future `Send`?" is answered for a body.
- `layout_of`: computing the size and layout of a type. Large nested futures make this busy.
- `LLVM_module_codegen_emit_obj`, `LLVM_module_optimize`, `codegen_module`: backend work. If these dominate, look at generics and optimization settings.
- `link`, `link_binary`: linking.

`crox` converts a profile into the Chrome trace format, which you can open in `chrome://tracing` or in [Perfetto](https://ui.perfetto.dev) to see the nesting on a timeline. `flamegraph` produces a flame graph SVG. Both are useful for seeing which function bodies dominate, especially with `args` recorded, because each event carries the name of the function (or the predicate) it was working on. With a few lines of `jq` over the `crox` output you can group `evaluate_obligation` events by predicate, or group `check_coroutine_obligations` by the function they were checking. That's exactly how the case study found its cause.

`cargo faster check-cost <crate>` runs this whole procedure for one crate, prints the top queries, and attributes trait-solving time to the function bodies and types involved.

### Counting LLVM lines

If your timeline shows long codegen sections, the question becomes: what is LLVM compiling? [`cargo-llvm-lines`](https://github.com/dtolnay/cargo-llvm-lines) answers it by counting lines of LLVM IR per function, after monomorphization:

```console
$ cargo install cargo-llvm-lines
$ cargo llvm-lines --lib -p my-app | head -12
  Lines                 Copies               Function name
  -----                 ------               -------------
  412031                9876                 (TOTAL)
   21788 (5.3%)          612 (6.2%)          core::ptr::drop_in_place
   14210 (3.4%)           88 (0.9%)          my_app::handlers::dispatch
    9876 (2.4%)          214 (2.2%)          <serde_json::de::...>::deserialize_struct
    ...
```

(The numbers here are illustrative.) Two columns: *Lines* is how much LLVM IR all instances of that function produced together, and *Copies* is how many instances there were. A generic function with hundreds of copies is being monomorphized for hundreds of types. A function with few copies but many lines is just big, and if it's generic, every copy is big. We'll see what to do about both in the section on generic code.

### Measuring fairly

Build times are noisy. A few habits will save you from fooling yourself:

- **Measure before and after back to back**, on the same machine, under the same load. A measurement from this morning compared with one from this afternoon tells you about this afternoon.
- **Know whether you're measuring cold or warm.** A *cold* build starts from an empty `target/` directory and empty caches. A *warm* build has most of the work done and only rebuilds what changed. The thing you care about day to day is usually the warm case after a realistic edit, like changing a function body in the crate you edit most. Decide which one you're measuring and keep it the same for before and after.
- **Make the same edit each time.** For warm measurements, a reliable way is to append a comment or a trivial function to one specific file, build, then revert.
- **Wall time under load overstates.** If other builds are running, every unit takes longer, and the timeline's numbers grow with them. Note the load average when you measure (`uptime`). Comparisons under similar load are still meaningful; absolute numbers less so.
- **Run it twice.** If two runs of the same thing differ by 20%, a 10% improvement is not something you can see.

## Trait solving and async code

In a lot of large async Rust codebases, the most expensive phase is not LLVM and not proc macros. It's trait solving, and specifically, proving that futures are `Send`. This section explains why that happens and what you can do about it. It can seem mysterious at first, so we'll build it up from the beginning.

### Why Send proofs get expensive

Let's start with what an `async fn` actually is:

```rust
async fn handle(bot: Arc<Bot>, room: RoomId) -> Result<()> {
    let members = bot.client.members(&room).await?;
    let reply = render(&members);
    bot.client.send(&room, reply).await?;
    Ok(())
}
```

Calling `handle` doesn't run it. It returns a future, which is a state machine. The compiler generates an anonymous type for it, with one state for each `.await` point. Each state holds whatever is still alive at that point: arguments, local variables, and the futures being awaited. Conceptually, it's something like this:

```rust
enum HandleFuture {
    Start { bot: Arc<Bot>, room: RoomId },
    AwaitingMembers { bot: Arc<Bot>, room: RoomId, fut: MembersFuture },
    AwaitingSend { bot: Arc<Bot>, room: RoomId, fut: SendFuture },
    Done,
}
```

The compiler calls this a *coroutine*, and the set of types held across await points its *witness* types.

Now, `tokio::spawn` requires its future to be `Send`, because the task might move between threads. So does `#[async_trait]` by default, and so do many other APIs. Is `HandleFuture` `Send`?

`Send` is an *auto trait*. Nobody writes `impl Send for HandleFuture`. Instead, the compiler decides structurally: a type is `Send` if all of its fields are `Send`. For a coroutine, the "fields" are its witness types. So to prove `HandleFuture: Send`, the compiler must prove:

- `Arc<Bot>: Send`, which requires `Bot: Send + Sync`,
- `RoomId: Send`,
- `MembersFuture: Send`, which is itself a coroutine, with its own fields,
- `SendFuture: Send`, likewise.

And `Bot: Send` means every field of `Bot` must be `Send`, and every field of those fields, all the way down. If `Bot` holds an HTTP client, which holds a connection pool, which holds a `HashMap` of connection states, each holding channels and buffers and `Arc<Mutex<...>>`s, the compiler walks all of it. A large SDK client can have hundreds or thousands of distinct types in its graph.

The compiler does cache these results. But here's the catch that makes it expensive in practice: the cache for proving a goal like `Bot: Send` is keyed not just by the goal, but by the context it's proved in: the *parameter environment* (the where-clauses in scope) and the *typing mode* (which, when checking a body that defines opaque types like `impl Future`, includes those opaque types). Two different function bodies usually have different environments. So each async function body that holds a `Bot` across an await proves `Bot: Send` again from scratch, walking the whole graph again.

How do you know if this is your problem? Self-profile the slow crate, as above. The signature is: `evaluate_obligation` with the largest self time, and `check_coroutine_obligations` and `mir_borrowck` with large total times. With `args` recorded, you can group the `evaluate_obligation` events by predicate and see the same `SomeType: Send` showing up dozens of times.

The general fix is to give the compiler less to prove. There are two ways: make the type graph smaller by hiding part of it behind a trait object, and stop futures from nesting their full types into each other by boxing them at a boundary.

### Erasing a heavy type

Here's a struct that holds an SDK client directly:

```rust
pub struct Mx {
    client: matrix_sdk::Client,
    user_id: OwnedUserId,
}

pub struct Bot {
    mx: Mx,
    db: PgPool,
    // ...
}
```

Every time the compiler proves `Bot: Send`, it proves `Mx: Send`, which proves `matrix_sdk::Client: Send`, which walks the client's entire internal state.

Now let's hide the client behind a trait object:

```rust
use std::fmt::Debug;
use std::sync::Arc;

pub trait SdkClient: Debug + Send + Sync {
    fn sdk(&self) -> &matrix_sdk::Client;
}

impl SdkClient for matrix_sdk::Client {
    fn sdk(&self) -> &matrix_sdk::Client {
        self
    }
}

pub struct Mx {
    client: Arc<dyn SdkClient>,
    user_id: OwnedUserId,
}

impl Mx {
    pub fn new(client: matrix_sdk::Client, user_id: OwnedUserId) -> Mx {
        Mx { client: Arc::new(client), user_id }
    }

    pub fn client(&self) -> &matrix_sdk::Client {
        self.client.sdk()
    }
}
```

What changed? To prove `Arc<dyn SdkClient>: Send`, the compiler needs `dyn SdkClient: Send + Sync`. And for a trait object, that's a matter of *declaration*: `SdkClient` has `Send + Sync` as supertraits, so every `dyn SdkClient` is `Send + Sync`, full stop. The compiler doesn't look inside. The type that's actually behind the pointer was proved `Send + Sync` exactly once, at the point where `Arc::new(client)` was coerced to `Arc<dyn SdkClient>`, and that proof isn't repeated.

You can write the same thing without a named trait, as `Arc<dyn Any + Send + Sync>` or with any trait that fits, as long as `Send + Sync` is part of the object type: `Arc<dyn MyTrait + Send + Sync>`.

The cost at runtime is one pointer indirection and a vtable call for `sdk()`, which is almost certainly lost in the noise next to a network client. The cost in code is a small wrapper and an `.sdk()` here and there.

One caution: this only helps types that *hold* the erased value. If an async function borrows a `&matrix_sdk::Client` and holds that borrow across an `.await`, its future contains `&Client`, and proving that `Send` requires `Client: Sync`, the full walk again. Erasure removes the type from the graph of `Bot`; it doesn't remove it from futures that mention it directly. That's what the next technique is for.

### Boxing futures at a boundary

Remember that an async function's future contains the futures it awaits. So if `a` awaits `b`, which awaits `c`, which awaits `d`, then `a`'s state machine contains `b`'s, which contains `c`'s, which contains `d`'s. The type of `a`'s future includes, by nesting, everything that any of them hold across an await. When the compiler proves `a`'s future is `Send`, it walks all of it, and it does that again in every async body that awaits `a`.

You can cut that nesting by returning a boxed future:

```rust
use std::future::Future;
use std::pin::Pin;

// Before: callers' futures contain the whole state machine of `connect`,
// including the SDK's internal futures.
pub async fn connect(cfg: &Config) -> Result<Client, Error> {
    let client = Client::builder().homeserver_url(&cfg.url).build().await?;
    client.restore_session(cfg.session.clone()).await?;
    Ok(client)
}

// After: callers hold an opaque, boxed future that is Send by declaration.
pub fn connect(cfg: &Config) -> Pin<Box<dyn Future<Output = Result<Client, Error>> + Send + '_>> {
    Box::pin(async move {
        let client = Client::builder().homeserver_url(&cfg.url).build().await?;
        client.restore_session(cfg.session.clone()).await?;
        Ok(client)
    })
}
```

The body is identical; it's just wrapped in `Box::pin(async move { ... })`. Callers still write `connect(&cfg).await`. But now what a caller's future holds is a `Pin<Box<dyn Future + Send>>`, and that type is `Send` by declaration. The compiler proves the inner future `Send` once, inside `connect`, where it's coerced to the trait object, and never again in the callers.

If you use the `futures` crate, `futures::future::BoxFuture<'a, T>` is a type alias for exactly this, and `FutureExt::boxed` does the `Box::pin` for you:

```rust
use futures::future::{BoxFuture, FutureExt};

pub fn connect(cfg: &Config) -> BoxFuture<'_, Result<Client, Error>> {
    async move {
        // ...
    }
    .boxed()
}
```

The runtime cost is one heap allocation per call and a dynamic call per poll. For a function you call once per connection or once per request, that's nothing. For a tight loop, measure first.

Where should you put these boundaries? Look for async functions that are called from many places and that await something heavy: SDK calls, database transactions, HTTP clients. The self-profile, grouped by the body being checked, tells you which callers are paying. The chain of awaits from those bodies down to the heavy call tells you where a box would cut it off.

### The cost of async-trait

Before Rust 1.75, you couldn't write `async fn` in a trait, and the `async-trait` crate filled the gap. It's still everywhere. Here's what it does with a method:

```rust
#[async_trait]
pub trait Adapter {
    async fn invite(&self, user: &UserId) -> Result<()>;
}
```

The macro rewrites it into something like this:

```rust
pub trait Adapter {
    fn invite<'life0, 'life1, 'async_trait>(
        &'life0 self,
        user: &'life1 UserId,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait;
}
```

and each implementation's body is wrapped in `Box::pin(async move { ... })`.

So every `#[async_trait]` method already returns a boxed future. That's good: callers don't nest the method's state machine. But look at the `where` clause. Every method gets lifetimes of its own, `'life0` and `'life1` and `'async_trait`, and bounds between them. Those bounds are part of the method's *parameter environment*: the set of facts rustc may assume while it checks the body.

Here's why that matters. When rustc proves `Bot: Send`, it caches the answer, and the cache key is the predicate *together with* the parameter environment it was proven in. Two bodies with the same environment share answers. Two bodies with different environments don't, even if the difference is a lifetime bound that has nothing to do with `Bot`. And `#[async_trait]` makes every method's environment different. So the boxed `+ Send` future of each method body proves its own `Send` facts about the same types, from scratch, in every impl. If fifty methods each hold an `Arc<Bot>` across an await, `Bot: Send + Sync` is proven fifty times.

Native `async fn` in traits, stable since Rust 1.75, doesn't box, and doesn't add those bounds. The method's future is an opaque type, and where `Send` is required, it's proven in an environment that looks like everyone else's, so the answers are shared. That's the first lever, and it's a mechanical one:

- In the trait, write `fn invite(&self, user: &UserId) -> impl Future<Output = Result<()>> + Send;` when callers spawn the future or otherwise need it to be `Send`. Plain `async fn` in the trait doesn't promise `Send`, and asking for it from a generic `T: Adapter` needs return type notation, which isn't stable yet.
- In the impls, keep writing `async fn invite(...)`. It satisfies the `impl Future + Send` signature.
- Native async traits aren't dyn-compatible. For traits you hold as `Arc<dyn Adapter>`, the `dynosaur` crate generates a `DynAdapter` wrapper (`#[dynosaur::dynosaur(DynAdapter)]`, then `DynAdapter::new_arc(v)`) that boxes the future only when you go through the `dyn`.
- Then keep the macro from coming back, with clippy:

```toml
# clippy.toml
disallowed-macros = [
    { path = "async_trait::async_trait", reason = "native async fn in traits; dynosaur for dyn" },
]
```

Clippy reports a disallowed attribute macro at the crate level, so an impl of someone else's `#[async_trait]` trait needs `#![allow(clippy::disallowed_macros)]` at the top of that crate until upstream moves.

The second lever is the one from [Erasing a heavy type](#erasing-a-heavy-type), and it's independent. Even with shared answers, each distinct environment and each async body that holds `Bot` across an await still proves `Bot: Send` at least once, and the cost of one proof is the size of `Bot`'s type graph. Dropping the macro cuts the number of proofs; erasing `Bot`'s heavy fields cuts the cost of each one. A workspace with both problems needs both fixes.

How do you tell which one you have? `cargo faster check-cost` counts the trait solving done in parameter environments with lifetime-outlives bounds, the shape `#[async_trait]` leaves behind, and reports it as `async-trait-overhead`. (The profile doesn't say `'async_trait`: rustc renames lifetimes when it builds cache keys. Hand-written `'a: 'b` bounds look the same, but they're rare in async code.) `send-proofs-per-async-body` is the other lever: one heavy type, proven over and over. [The case study](#dropping-async-trait) has numbers for the first; [the earlier fixes](#fix-1-erase-the-client) are the second.

## Generic code and monomorphization

Generic functions are compiled once per set of concrete types they're used with. That's what makes them fast at runtime, and it's what can make them slow at compile time.

Here's a function from the standard library, more or less:

```rust
pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}
```

Call it with a `&str`, a `String`, a `PathBuf`, and a `&Path`, and you get four copies of the whole function, each compiled by LLVM, even though only the first line depends on `P`. The real standard library avoids this with a trick:

```rust
pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    fn inner(path: &Path) -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    inner(path.as_ref())
}
```

Now the generic part is a one-line shim, and the real work is in a non-generic `inner` function that's compiled exactly once. Use this pattern for any generic function with a large body where the generic parameter is only used for a conversion at the start. `cargo llvm-lines` will point you at the candidates: functions with many copies and many lines.

The other lever is choosing between generics and trait objects. A function that takes `impl Fn(Event)` or `T: Handler` is monomorphized for every closure or handler type. A function that takes `&dyn Fn(Event)` or `&dyn Handler` is compiled once. The trait object costs a dynamic call at runtime, which matters in hot loops and rarely elsewhere. Code at the edges of your system, like configuration, setup, request routing, and error handling, can often take `dyn` without anyone ever noticing at runtime, and your compile time will notice.

A few other common sources of monomorphization bloat:

- **`drop_in_place`** for large types shows up in almost every `cargo llvm-lines` report. Every distinct type with drop glue gets its own copy. Deeply nested generic types, and especially large async state machines, produce lots of them.
- **Serialization.** `serde`'s derived `Deserialize` impls are generic over the deserializer, so they're instantiated once per format you use them with.

Monomorphization happens in the crate that *uses* the generic function with a concrete type, not in the crate that defines it. So a generic-heavy library can be quick to compile on its own and still make every crate that uses it slow to build. When the timeline shows a crate with a long codegen section and not much code of its own, that's often what's happening.

## Procedural macros

A procedural macro is a Rust program that runs during compilation. It takes tokens and returns tokens. Because it runs inside the compiler, it has to be compiled first, for the *host* (the machine doing the build), as a dynamic library that `rustc` loads.

This has a few consequences for build time.

First, proc macro crates and their dependencies are always compiled to native code, even under `cargo check`. Most proc macros depend on `syn`, `quote`, and `proc-macro2`. `syn` in particular is a large crate, and with its `"full"` feature enabled, which it needs to parse arbitrary Rust items and expressions, it's larger still. You'll usually see it near the start of your timeline.

Second, proc macro crates break pipelining. Downstream crates can't start when the proc macro's metadata is ready; they wait for the whole thing to be built and linked. If `serde_derive` takes several seconds, then `serde` with the `derive` feature can't finish until it does, and everything that depends on `serde` waits too. That puts `syn` and `serde_derive` on the critical path of a lot of builds.

Third, running the macros takes time in the frontend of every crate that uses them. `#[derive(Serialize, Deserialize)]` on 300 structs means running the derive 600 times and then type checking and compiling the generated code, which is usually much larger than what you wrote. In a self-profile, the time spent running macros shows up as `expand_proc_macro`; the cost of what they generated shows up in all the later phases.

What can you do?

- **Check whether proc macros are actually your problem.** In many profiles, `expand_proc_macro` is a fraction of a percent. In the case study profile above, it was 76 ms out of 38 seconds. Don't remove derives on a hunch.
- **Look at the critical path.** If `syn` and a derive crate sit at the start of a long chain, that's early wall time you pay on every cold build. Cached builds avoid it, so this matters most for CI and fresh checkouts.
- **Avoid deriving what you don't use.** A `Deserialize` on a type you only ever serialize is generated code nobody needs.

## The shape of your workspace

Your dependencies are mostly cached after the first build. Your own crates are the ones you rebuild all day. So the way you've arranged them matters a great deal.

### The critical path

We met the critical path in the timeline section: the longest chain of dependent units in the build. In a workspace, it often runs right through your own crates: `app` depends on `api`, which depends on `services`, which depends on `db`, which depends on `models`. If each is a few seconds to check, the chain is the sum.

Look at your timeline after a typical edit. The units that rebuilt are the ones on the path from the crate you edited to the final binary or test. If they form a long staircase, every link in that chain is directly adding to your wait. Making one crate in the chain faster, or making the chain shorter, both help.

### Rebuild fan-out

When you edit a crate, Cargo rebuilds that crate and every crate that depends on it, directly or indirectly: its *reverse dependencies*. (Strictly, if the crate's public metadata didn't change, some of those rebuilds can be quick, but Cargo still has to run `rustc` on each of them, and with incremental compilation the dependents often still redo a lot.)

That gives you a simple rule: **the crates you edit most should have the fewest reverse dependencies**. Put frequently edited code near the leaves of the graph, in crates that few others depend on, like the application binary or individual feature crates. Put stable code, that changes rarely, at the root: shared types, error definitions, utility traits. These *foundation crates* are cheap as long as they stay stable, and very expensive whenever they change, because every edit rebuilds everything above them.

`cargo faster rdeps <crate>` lists what rebuilds when you edit a given crate, and how long those units took in your last timeline.

### Splitting a big crate

Splitting a big crate into several smaller ones can help in two ways:

- **Parallelism.** `rustc`'s frontend is mostly single-threaded on stable (see the parallel frontend below). A 200,000-line crate is checked on one core. Two 100,000-line crates that don't depend on each other can be checked on two.
- **Smaller rebuilds.** If you edit code in one half, only that half and its dependents rebuild.

It doesn't help when the pieces end up depending on each other in a chain. If `big` becomes `big-a` and `big-b`, where `big-b` depends on `big-a`, you haven't gained any parallelism, and an edit in `big-a` still rebuilds both. It also doesn't help if the cost isn't proportional to code size. If one function is taking 30 seconds because of trait solving, moving it into its own crate moves the 30 seconds with it. And it doesn't help much with codegen-heavy crates if the expensive generic instantiations happen downstream anyway.

So before splitting, profile. If the time is spread fairly evenly across many functions and modules, and you can draw a line through the crate where the two sides don't need each other, a split is likely to pay off. If the time is concentrated, fix the concentration.

## Dependencies and features

Most of the code you compile on a cold build isn't yours. A few habits keep that under control.

**Turn off default features you don't need.** Many crates enable a lot by default. `reqwest`'s defaults include a TLS backend and more; maybe you wanted `rustls` instead and are now compiling both. Use `default-features = false` and list what you need:

```toml
[dependencies]
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

**Be careful with "full".** `tokio = { features = ["full"] }` is convenient, and compiles everything: the multi-threaded runtime, process, signal, file system, and more. `syn = { features = ["full"] }` compiles a much larger parser. Often you need only a few features. Remember that features are additive across the whole build: if any crate in your graph turns on a feature, everyone gets it. So check what's turning things on before you remove them.

To see which features are enabled and who enabled them, use `cargo tree -e features`:

```console
$ cargo tree -e features -i tokio
tokio v1.41.0
├── tokio feature "default"
│   ├── my-app v0.1.0 (/home/you/proj/my-app)
│   └── ...
├── tokio feature "full"
│   └── my-app v0.1.0 (/home/you/proj/my-app)
...
```

`-i tokio` inverts the tree, so you see who depends on `tokio` and through which features.

**Watch for duplicate versions.** If two of your dependencies need different major versions of the same crate, you compile both. `cargo tree -d` (short for `--duplicates`) lists them:

```console
$ cargo tree -d
syn v1.0.109
└── some-old-derive v0.3.2 (proc-macro)
    └── my-app v0.1.0 (/home/you/proj/my-app)

syn v2.0.87
├── serde_derive v1.0.215 (proc-macro)
...
```

You can't always fix duplicates, since you don't control your dependencies' requirements. But sometimes an upgrade of one dependency, or replacing a small, old one, removes a whole second copy of `syn` or `hyper` from your build. Duplicates of big crates are the ones worth chasing. Looking at `Cargo.lock` for packages with more than one entry tells you the same thing.

**Count the proc macros.** Every proc macro crate is compiled for the host and blocks pipelining for its dependents. Several crates with small optional `derive` features each pulling in `syn` can add up.

`cargo faster deps` reports heavy features, duplicate versions, and proc macros in your graph. Duplicate versions are listed in semver order, oldest first, since the oldest copy is usually the one to chase.

## Caching

The fastest compilation is the one that doesn't happen. Rust has several layers of caching, and they behave in ways that aren't always obvious.

### Incremental compilation

Incremental compilation saves the results of `rustc`'s queries between builds, in `target/<profile>/incremental/`. On the next build of the same crate, `rustc` figures out which inputs changed, and reuses every query result that didn't depend on them. It also reuses the object files of codegen units whose contents didn't change.

Some things to know about it:

- **It's on by default for the `dev` and `test` profiles, and off for `release`.** You can change that with `incremental = true` or `false` in a profile, or override it with `CARGO_INCREMENTAL=0` or `1`.
- **It's only used for workspace members and path dependencies.** Crates from crates.io are compiled once and not recompiled when you edit your code, so there's nothing to gain.
- **It's still a rerun of the crate.** `rustc` still has to parse and expand the whole crate, and check what changed. For a big crate, an incremental rebuild after a small edit is much faster than a full one, but not free.
- **It's tied to its target directory.** The saved state lives inside `target/` and refers to your sources and output by path. Copying it into a different checkout doesn't reliably give you incremental reuse; we'll come back to this below.
- **It interacts with caching wrappers.** Most compiler caches can't cache incremental compilations, as the next section explains.

### sccache and kache

`sccache` and `kache` are compiler wrappers. Cargo lets you set a program that runs instead of `rustc` and receives the `rustc` command line:

```toml
# .cargo/config.toml
[build]
rustc-wrapper = "sccache"
```

or `RUSTC_WRAPPER=sccache` in the environment. The wrapper computes a key from everything that could affect the output: the source files, the compiler version, the flags, the environment variables the crate reads, and the dependencies' outputs. If it has seen that key before, it copies the cached output into place and `rustc` never runs. Otherwise it runs `rustc` and stores the result.

They cache *whole crate compilations*. That has consequences:

- **They can't cache incremental crates.** An incremental compilation's output depends on the state of the incremental directory, not just the inputs, so wrappers pass these through to `rustc` uncached. In practice that means your workspace crates under `dev` aren't cached, and your dependencies are. For a typical project that's still a big win on cold builds, since dependencies are most of the code.
- **Any change to the inputs is a miss.** Different compiler version, different `RUSTFLAGS`, different enabled features, different absolute paths (unless the wrapper normalizes them): different key.
- **Feature unification can split your keys.** With the version 2 resolver (the default since the 2021 edition), Cargo resolves features for the set of packages you ask it to build. `cargo build -p foo` unifies features only across what `foo` needs; `cargo build --workspace` unifies across every member. If some other member enables `tokio/fs`, then `tokio` is compiled with different features in the two commands, so it has a different key, and the cached copy from one doesn't help the other. Cargo's own `target/` has the same issue, and you'll see dependencies rebuild when you switch between the two. The fix is consistency: build the same set of packages you always build, with the same flags, everywhere, including CI and local scripts.

When you're debugging a cache, check its statistics. `sccache --show-stats` reports hits and misses. A good warm cache in a fresh checkout should be a high hit rate, with the misses being exactly the crates you expect: your incremental workspace crates and anything you changed.

`cargo faster cache` shows which wrapper is configured, whether incremental is on for which crates, and where your target directories live.

### Worktrees and cold target directories

Git worktrees are a great way to work on several branches at once. But each worktree is a separate directory with its own `target/`, so the first build in a new worktree is cold: every dependency compiles from scratch, as if you'd just cloned the repository.

There are a few ways to soften this:

- **Use a compiler cache.** This is the most effective option. Dependencies compiled in one worktree are cache hits in the next. Your incremental workspace crates will still compile, since they can't be cached.
- **Share a target directory** across worktrees, with `CARGO_TARGET_DIR` or `build.target-dir` in a config file. This avoids recompiling dependencies at all, but builds in different worktrees now contend for the same directory (Cargo takes a lock, so they'll wait for each other), and switching between branches with different code means incremental state and fingerprints keep getting invalidated. This works best when you only build in one worktree at a time.
- **Don't count on copying incremental state.** It's natural to think you could seed a new worktree with the incremental directory from another one, at the same commit, and skip the cold build of your own crates. In our measurements (in the case study) that made builds *slower*, not faster: the compiler did the work anyway, plus the work of loading and discarding the imported state.

## Linking

Every binary, test binary, example, and benchmark is linked separately. Linking reads all the object files and libraries involved, resolves symbols, and writes the output. For a large Rust program with debug info, that's a lot of data, and with the traditional GNU linker `ld.bfd` it could take many seconds per binary.

The fix is a faster linker, and it may already be done for you: **since Rust 1.90, `rustc` uses LLD, LLVM's linker, by default on `x86_64-unknown-linux-gnu`.** If you're on that target with a recent compiler, you're already using a fast linker. On other targets and older compilers, you choose.

Two other linkers are worth knowing on Linux:

- [**mold**](https://github.com/rui314/mold) is designed for speed and is usually faster than LLD for large outputs.
- [**wild**](https://github.com/davidlattimore/wild) is a newer linker written in Rust, aiming for even faster incremental linking. It's younger than mold; check its README for current status and setup.

You set the linker per target in `.cargo/config.toml`. With mold, the usual approach is to run the link through `clang` and tell it which linker to use:

```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

Or, without changing your config, run Cargo under mold: `mold -run cargo build`.

On macOS, Apple's newer linker (introduced with Xcode 15, sometimes called `ld-prime`) is much faster than the old one and is the default with current Xcode. If linking on macOS is slow, check that your Xcode command line tools are up to date.

The other big factor is **debug info**. Debug info is often most of the size of a debug binary, and the linker has to process all of it. If linking dominates your timeline even with a fast linker, reduce debug info, as described in the next section. And remember that every test binary is linked separately, which matters a lot for test suites with many integration test files.

`cargo faster setup` checks which linker you're using, along with your profile settings.

## Profile settings

Cargo profiles control how your code is compiled. The defaults favor a good debugging experience in `dev` and a fast program in `release`. You can trade some of that away for faster builds.

### Debug info

By default, the `dev` profile produces full debug info (`debug = true`, the same as `debug = 2` or `"full"`). That includes everything a debugger needs to show you variables and their types. It makes code generation and linking slower and binaries much bigger.

If you don't use a debugger regularly, you can reduce it:

```toml
[profile.dev]
debug = "line-tables-only"
```

`"line-tables-only"` (stable since Rust 1.71) keeps file and line information, so panics and backtraces still tell you where things happened, but drops the information about variables and types. `debug = 0` (or `false`) drops debug info entirely; backtraces will show function names but not line numbers.

You can also keep full debug info for your own code but reduce it for dependencies:

```toml
[profile.dev.package."*"]
debug = "line-tables-only"
```

`split-debuginfo` controls whether debug info is kept in the object files and linked into the binary, or kept separately. On macOS the default for `dev` is `"unpacked"`, which leaves debug info in the object files so the linker doesn't need to process it. On Linux, `split-debuginfo = "unpacked"` (stable since Rust 1.65) does something similar using separate `.dwo` files, which can make linking noticeably faster. Debuggers and tools need to support split DWARF to find the information, and recent `gdb` and `lldb` do.

The tradeoff is the debugger experience. If you reach for `gdb` or `lldb` once a month, consider switching to a profile with full debug info for that day:

```toml
[profile.debugging]
inherits = "dev"
debug = true
```

and run `cargo build --profile debugging`.

### Optimizing dependencies in dev builds

Sometimes debug builds are too slow at *runtime*: tests that do a lot of computation, image processing, parsing, cryptography. A common fix is to optimize your dependencies, while keeping your own code unoptimized:

```toml
[profile.dev.package."*"]
opt-level = 2
```

`"*"` matches every package that isn't a workspace member. This makes clean builds slower, since LLVM does more work on all your dependencies. But dependencies are compiled once and then reused, so you pay it on cold builds, not on each edit. It doesn't affect incremental compilation of your own crates, which stay at `opt-level = 0`.

One subtlety: generic code from dependencies is compiled in the crate that instantiates it. A `HashMap<MyKey, MyValue>` used in your crate is monomorphized and compiled in your crate, at your crate's opt level. So this setting speeds up code that's fully compiled in the dependency, and helps less for generic-heavy libraries.

Whether `opt-level = 1`, `2`, or `3` is best depends on the code. `1` is a cheap middle ground. Measure your test suite with each.

### Codegen units

LLVM works on *codegen units*: chunks of a crate that can be optimized and compiled in parallel. More units means more parallelism and faster compiles, but fewer optimization opportunities, since LLVM can't inline across unit boundaries without LTO.

The defaults are 256 for incremental builds (so most of the `dev` profile) and 16 for non-incremental builds such as `release`. Raising `codegen-units` in `release` can speed up release builds at some cost in runtime performance. People often set `codegen-units = 1` in release for the best runtime performance; that serializes LLVM's work for each crate and can make release builds much slower. If you need it, consider putting it in a separate profile, like `dist`, used only for the builds you ship.

### Sharing generics

When two crates both use `Vec<u32>::push`, each might compile its own copy. With *shared generics*, a downstream crate reuses the instantiation an upstream crate already compiled, instead of generating its own.

Shared generics are already on by default for unoptimized builds (`opt-level = 0`, and also `1`, `"s"`, and `"z"`), so in a normal `dev` build you're getting them without doing anything. They're off by default at `opt-level = 2` and `3`, because duplicating instantiations lets LLVM inline and optimize them locally. You can force them on with `-Zshare-generics=y`, which is a **nightly-only** flag. Where that might matter is the setup from the previous section: dependencies at `opt-level = 2` or `3` in a dev build. Measure, as always; it's not obviously a win.

### The parallel frontend

On stable Rust, `rustc`'s frontend (parsing, type checking, borrow checking) runs on a single thread per crate. Code generation is already parallel across codegen units. For a big crate, that single-threaded frontend is often the critical path.

Nightly `rustc` has a parallel frontend, enabled with `-Zthreads=N`:

```console
$ RUSTFLAGS="-Zthreads=8" cargo +nightly check
```

This is **nightly-only** at the time of writing. It can noticeably speed up big crates, especially on a machine that's otherwise idle; on a machine where Cargo is already running many crates in parallel, there are fewer spare cores to use, and the benefit is smaller. Note that setting `RUSTFLAGS` changes the build flags of every crate, which invalidates your caches and causes a full rebuild.

## Test suites

`cargo test` builds your tests and then runs them. Both halves can be slow, and a few setup choices affect both.

### nextest

[`cargo-nextest`](https://nexte.st) is an alternative test runner. Where `cargo test` runs each test binary in turn, with the tests in that binary running on threads, nextest runs **each test in its own process**, and runs tests from all binaries in parallel. A slow test in one binary no longer holds up the others, and a test that crashes or leaks state can't affect its neighbors.

```console
$ cargo install cargo-nextest --locked
$ cargo nextest run --workspace
```

You control parallelism with `--test-threads` (or `-j`), or in profiles in `.config/nextest.toml`:

```toml
[profile.default]
test-threads = "num-cpus"
slow-timeout = { period = "30s", terminate-after = 4 }

[profile.ci]
test-threads = 8
fail-fast = false
```

`cargo nextest run --profile ci` picks a profile. Individual tests can declare that they need more than one slot, with `threads-required` in a per-test override, which is useful for tests that themselves use many threads.

One trap: **the `NEXTEST_TEST_THREADS` environment variable takes precedence over the profile's `test-threads`.** If it's set in your shell profile, your CI environment, or a wrapper script, your config file's setting is silently ignored. If your test parallelism doesn't seem to match your configuration, check your environment.

### Test runners that wrap every test

Cargo lets you configure a *runner* that wraps every executable it runs for a target:

```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-gnu]
runner = "my-wrapper"
```

or the environment variable `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER`. This is meant for things like running tests under an emulator, or on a remote device. Nextest honors it too.

Under `cargo test`, the runner wraps each test *binary*, once. Under nextest, which runs one process per test, it wraps every *test*. A runner that takes a second to start, or that takes a lock, or that coordinates with some other service, now does that thousands of times. Each wrapped test also holds one of nextest's slots while it waits. In the case study, tests that ran in 0.00 seconds were each waiting 3 to 8 seconds in a runner, and that waiting was about 44% of all the test-seconds in the suite.

If you have a runner configured, check whether it's needed for tests, and whether it's set somewhere you didn't expect: a global `~/.cargo/config.toml`, or an environment variable. `cargo faster tests` reports runners, nextest configuration, and relevant environment variables.

### Database-backed tests

Tests that talk to a real database are often slow for reasons that have nothing to do with Rust. If your tests use PostgreSQL, a few changes can make a big difference:

- **Put the test cluster's data on `tmpfs`.** A test database doesn't need to survive a reboot. Keeping its data directory in memory makes every write fast.
- **Turn off durability for the test cluster.** Settings like `fsync = off`, `synchronous_commit = off`, and `full_page_writes = off` tell Postgres not to wait for the disk. Never do this for data you care about; for a test cluster, it's exactly right.

  ```console
  $ postgres -D /dev/shm/pgtest -c fsync=off -c synchronous_commit=off -c full_page_writes=off
  ```

- **Clone a template instead of migrating each database.** A common pattern, and the one `#[sqlx::test]` uses, is to give each test its own fresh database so tests can run in parallel without interfering. If each of those databases is created by running all your migrations, then with 100 migrations and 1,000 tests you're running 100,000 migrations. Instead, migrate one database once, and create each test database as a copy of it:

  ```sql
  CREATE DATABASE test_4711 TEMPLATE app_template;
  ```

  Copying a template is a file-level copy, and is usually much faster than replaying migrations. (Postgres requires that nobody else is connected to the template while it's being copied.)

### Fewer test binaries

Each file in `tests/` is compiled as its own crate and linked as its own binary. Ten files means ten crates that each depend on your library and all its dependencies, and ten link steps.

```text
tests/
├── api.rs
├── auth.rs
├── billing.rs
└── ...
```

You can turn them into one binary with modules:

```text
tests/
└── it/
    ├── main.rs
    ├── api.rs
    ├── auth.rs
    └── billing.rs
```

```rust
// tests/it/main.rs
mod api;
mod auth;
mod billing;
```

Cargo picks up `tests/it/main.rs` as a single integration test target named `it`. Now there's one crate to compile and one binary to link. You can still run a subset by name: `cargo test --test it auth::`, or with nextest's filters.

## The machine

Finally, the build runs on a computer, and that computer may be busier than you think.

**Load versus cores.** Run `uptime` or look at the load average in `top`. If the load average is 45 on a 32-core machine, every process is getting less than a full core, and your build is waiting in line. This is common on shared build machines, and on development machines running several editors' language servers, a test suite, and two builds in different worktrees at once. Every timing you take under that load is inflated.

**Competing builds.** `rust-analyzer` runs `cargo check` in the background when you save, often with its own target directory. Two builds in the same target directory wait for each other on Cargo's lock ("Blocking waiting for file lock on build directory"). Two builds in different directories compete for CPU.

**Job count.** Cargo runs as many jobs as you have logical CPUs by default. You can change that with `-j N`, `CARGO_BUILD_JOBS`, or `build.jobs` in config. On a machine you share, a lower number can make your build a better neighbor. On a machine with little memory, it can prevent swapping.

**Memory.** Each `rustc` process can use a gigabyte or more for a big crate, and linking can use more. If the machine starts swapping, everything slows down dramatically. Check with `free -h` or `vmstat 1` during a build. If you see swap activity, reduce parallelism, reduce debug info, or add memory.


**Disk.** `target/` directories grow large with several worktrees and profiles, and a full disk fails builds in confusing ways. `cargo clean` reclaims it all.

**Same filesystem.** Compiler caches and Cargo copy a lot of files. When the cache directory and the target directory are on the same filesystem, a cache can use hard links or, on filesystems like Btrfs and XFS, reflinks (copy-on-write clones), which are nearly instant. Across filesystems, every hit is a full copy of the data. Keep `target/` and your cache directory on the same filesystem where you can.

`cargo faster machine` reports load, memory, swap, and disk, and whether your cache and target directories share a filesystem.

## Case study: a 312 second check

Here's a story from a real workspace: about forty crates of async Rust, implementing a chat bot that talks to Matrix servers using `matrix_sdk`, with `tokio`, `reqwest`, and `sqlx` underneath. It's the case that most of this guide grew out of, and it's a good example of why measuring first matters.

### The symptom

The workspace had a low-level crate, `tala-mx`, that wrapped the Matrix client. After editing it, `cargo clippy --workspace --all-targets` took **12 minutes and 3 seconds**.

Edits to a low-level crate rebuild everything above it, so some of that was expected. But the timeline showed something that wasn't: one unit, the check of the application's binary crate, took **312 seconds**. The application's library crate took 298 seconds. Their test-harness checks were similar. These were `cargo check`-style units: no LLVM at all. Whatever was slow was in the frontend.

The first guesses were the usual ones. It's a big crate. It uses a lot of derives. Maybe it's the proc macros, or maybe the crate should be split.

### The measurement

We self-profiled the binary crate with a standalone `rustc` invocation, as described in [Self-profiling one crate](#self-profiling-one-crate): `-Zself-profile` with `args`, incremental off, no wrapper, `RUSTC_BOOTSTRAP=1` on the stable compiler we normally use. Under the machine's load, it took 380 seconds.

The answer was unambiguous. **`evaluate_obligation` accounted for 92 to 97% of the time**, nested under `check_coroutine_obligations` and `mir_borrowck`. Macro expansion was a rounding error. LLVM wasn't involved. This was trait solving, triggered by async bodies.

That tells you *what*, but not *why*. With `args` recorded, every `evaluate_obligation` event carries the predicate it was proving. Grouping by predicate showed the same handful over and over: `Bot: Send`, `Bot: Sync`, `Arc<Bot>: Sync`, `Weak<Bot>: Send`, and so on, **each proved 54 times**. `Bot` is the application's main struct; it holds everything.

Grouping by the crate of the type being proved showed where inside `Bot` the time went: `matrix_sdk` 4.1 s, `tokio` 2.6 s, `ruma` (Matrix types) 1.8 s, `reqwest` 1.2 s, `sqlx` 0.75 s. And attributing the work to the function bodies that asked for it, using the argument of each `check_coroutine_obligations` event, gave a list topped by two methods, `canonical_persona::Adapter::invite` at 8.20 s and `host::DaemonHost::invite` at 3.16 s, followed by dozens of `#[async_trait]` methods at around half a second each.

### The surprising answer

Here's what was happening.

`Bot` held an `Mx`, the wrapper from `tala-mx`. `Mx` held a bare `matrix_sdk::Client`. Inside that client is a large graph of types: an event cache, the latest events per room, a send queue, broadcast channels, and much more.

Every `#[async_trait]` method that touched `Bot` across an await produced a boxed future that had to be `Send`. Proving that meant proving `Bot: Send + Sync`, which meant walking all of `matrix_sdk::Client`'s internals. And as we saw in [The cost of async-trait](#the-cost-of-async-trait), each method's lifetime bounds are part of its parameter environment, which is part of the trait solver's cache key. So the proof wasn't shared between methods. About 130 async-trait methods across the crates each walked the whole graph again, each costing seconds under load.

The crate wasn't too big, and macros weren't the problem. A single field type, in a crate two levels down, was being re-proved `Send` over a hundred times.

### Fix 1: erase the client

The fix was a pull request of about thirty lines in `tala-mx`, the technique from [Erasing a heavy type](#erasing-a-heavy-type):

```rust
pub trait SdkClient: Debug + Send + Sync {
    fn sdk(&self) -> &Client;
}

impl SdkClient for Client {
    fn sdk(&self) -> &Client {
        self
    }
}

pub struct Mx {
    client: Arc<dyn SdkClient>,
    // ...
}
```

Now proving `Mx: Send` stops at `dyn SdkClient`, which is `Send + Sync` by declaration.

Here are the results. They were measured with `cargo clippy --timings` after the same edit to `tala-mx`, back to back, on a 32-core machine with a load average around 45:

| Unit | Before | After |
|---|---:|---:|
| app binary crate, check | 312 s | 49 s |
| app binary crate, check (test) | 326 s | 51 s |
| app library crate, check | 298 s | 76 s |
| app library crate, check (test) | 313 s | 95 s |
| another crate, check | 51 s | 7 s |
| another crate, check | 49 s | 8 s |
| **whole clippy run** | **12m03s** | **2m32s** |

The standalone self-profiled `rustc` went from 380 s to 52 s for the binary crate, and from 388 s to 79 s for the library.

### Fix 2: a boundary for the client's futures

Fix 1 removed `matrix_sdk::Client` from `Bot`'s type graph. But the profile still showed `matrix_sdk` types in the binary crate's proofs, and still had `Adapter::invite` at the top.

Following the chain of awaits from `Adapter::invite` explained it. `Mx::new` called the SDK's `ClientBuilder::build` and `restore_session` and awaited them inline. So their futures were part of `Mx::new`'s future, which was part of `owner_mx`'s, which was part of `after_create`'s, then `assistants_space`'s, then `persona_invite`'s, and finally of every async-trait body that reached it. Each of those bodies re-proved `Send` over the SDK's internal client state, field by field, because its futures were nested inside theirs.

The fix was the technique from [Boxing futures at a boundary](#boxing-futures-at-a-boundary): return those two futures as `Pin<Box<dyn Future<Output = ...> + Send>>`. That's stable Rust, with no `unsafe` and no newtype, and the function bodies didn't change.

These numbers come from a different situation than Fix 1's: a comment edited in the binary crate, then `cargo clippy --workspace --all-targets`, with the cache restoring everything else. So it's a mostly-incremental rebuild, and the starting point was already after Fix 1:

| Measurement | Before | After |
|---|---:|---:|
| `check_coroutine_obligations` for `Adapter::invite` | 8.20 s | 1.09 s |
| `check_coroutine_obligations` for `DaemonHost::invite` | 3.16 s | 0.60 s |
| `matrix_sdk` proofs in the binary crate | 4.9 s | 0 s |
| `ruma` proofs in the binary crate | 1.8 s | 0 s |
| `tokio` proofs in the binary crate | 2.6 s | 0.8 s |
| binary crate check | 28.4 s | 22.1 s |
| clippy wall time | 36.1 s | 27.1 s |

### What's left

After both fixes, about 15.6 seconds of the binary crate's remaining `evaluate_obligation` time is still `Bot: Send + Sync`. `Bot`'s graph is about 1,366 types: `reqwest` and the `hyper` and `tower` stack under it, `sqlx`, and the application's own subsystems. It's re-proved in 54 async bodies, at about 0.29 seconds each.

Would switching from `#[async_trait]` to native async functions in traits help? We guessed no: each body would still be its own environment, and would still prove `Bot: Send` for itself. We were wrong, and the next section is how we found out.

### Dropping async-trait

Some months later, the workspace replaced all of its `#[async_trait]` uses with native async functions in traits, over three pull requests: 84 traits and 381 impls. Traits that callers spawned got `-> impl Future<Output = T> + Send` signatures; traits held as `Arc<dyn ...>` got `dynosaur` wrappers; and a `disallowed-macros` entry in `clippy.toml` keeps the macro out.

The first measurement was crude. Append one new function to the library crate and one to the binary crate, then run `cargo clippy --workspace --all-targets`, so it's an incremental rebuild of the top of the graph. At a load average between 22 and 32 on the same 32-core machine:

| Unit | Before | After |
|---|---:|---:|
| app library crate, check | 44.0 s | 15 to 16 s |
| app binary crate, check | 25.7 s | 3 to 7 s |
| **whole clippy run** | **1m16s** | **26 to 31 s** |

That's a bigger win than the theory in [What's left](#whats-left) allowed for, and other changes landed in the same weeks, including more erasure inside `Bot`. So we went back and measured just the last of the three pull requests, the commit before it against the commit after, with `cargo faster check-cost`: a fresh, non-incremental self-profile of each crate, two runs each, back to back, under a load average of 50 to 77.

| Measurement | Before | After |
|---|---:|---:|
| library crate: trait solving (`evaluate_obligation`) | 4.4 to 5.2 s | 0.85 to 1.0 s |
| library crate: in lifetime-bounded environments | 3.4 to 3.7 s | 0 s |
| library crate: proving async bodies `Send` | 1.5 to 1.6 s | 0.07 to 0.09 s |
| library crate: `typeck` | 5.9 to 9.7 s | 4.7 to 5.5 s |
| library crate: borrow checking | 5.2 to 6.3 s | 3.9 to 4.7 s |
| library crate: whole check | 43 to 49 s | 26 to 38 s |
| binary crate: trait solving | 1.1 to 1.5 s | 0.41 to 0.49 s |
| binary crate: whole check | 9 to 10 s | 11 to 13 s |

Two things stand out. First, trait solving dropped by about 80%, and nearly all of what went away was work done in parameter environments carrying `#[async_trait]`'s lifetime bounds: 14,500 evaluations in the library crate before, none after. Native async functions really do let rustc share its answers. Our guess in *What's left* had the right model of each body and the wrong model of the cache.

Second, on a fresh check, trait solving was only about a tenth of the library crate's time to begin with, so the fresh numbers move less than the trait-solving ones, and the binary crate's fresh check didn't move outside the noise.

And the incremental probe, the one you wait on after every edit, repeated on this one commit pair, at a load average of 74 to 100 (the second of two runs in each worktree, so incremental state was warm):

| Unit (append a function, rebuild) | Before | After |
|---|---:|---:|
| `cargo check`: app library crate | 15.9 s | 13.3 s |
| `cargo check`: app binary crate | 3.1 s | 2.4 s |
| `cargo check`: whole run | 55 s | 46 s |
| `cargo clippy`: app library crate | 46.0 s | 47.2 s |
| `cargo clippy`: app binary crate | 10.4 s | 6.9 s |
| `cargo clippy`: binary crate (test) | 14.3 s | 10.4 s |
| `cargo clippy`: whole run | 83 s | 72 s |

That's a real win, but a modest one, and it's honest to say why. By the commit before the last pull request, only 62 `#[async_trait]` attributes were left, in 9 crates; the first two pull requests had already removed the rest. The first probe's 44 to 15 seconds spans all three pull requests *and* the erasure work that landed alongside them. We can't split that number between the two after the fact. What the self-profile does show is that the trait solving that `#[async_trait]` caused went to zero, and that it was most of the trait solving left.

So there are two levers, and this workspace needed both. Erasing heavy types made each `Send` proof cheap. Dropping `#[async_trait]` stopped the proofs from being repeated per method.

### Caching lessons

Along the way, we looked at caching too. The workspace used `kache`, a compiler cache along the lines of `sccache`.

- **Seeding incremental state across worktrees made things slower.** `kache` can copy the incremental state of selected crates from one worktree into another. We tried it with three worktrees at the same commit. The imported state was found and applied, but builds that used it took 179 and 204 seconds, against 139 seconds for a control without it. The compiler did the work anyway, plus the cost of the import. Clippy runs never seeded at all, because of how clippy wraps `rustc`.
- **Incremental compilation only paid off in the same worktree.** A repeat build in the same worktree after a small edit took 19 seconds. The same edit in a fresh worktree took around 140.
- **The cache was working.** In a fresh worktree, the hit rate was 98.4%: 922 hits and 11 misses. The misses were exactly the big incremental workspace crates, which a whole-crate cache can't store. Those few crates were the whole cold-build cost.

### Test suite lessons

The test suite had problems of its own, none of them in the compiler:

- **A runner wrapped every test.** A target runner (`kache test-runner`) was configured for all test processes. Under nextest, which runs one process per test, each test took a scheduler slot while the runner started and coordinated. Tests that ran in 0.00 seconds waited 3 to 8 seconds each, and that waiting added up to about 44% of all test-seconds.
- **An environment variable overrode the config.** `NEXTEST_TEST_THREADS` was set in the environment, which silently beat the `test-threads` setting in the nextest profile.
- **The database setup did unnecessary work.** Postgres data lived on disk rather than `tmpfs`, and each test database ran the migrations instead of being cloned from a template.

### Lessons

A few things to take away from this:

1. **The obvious suspects were wrong.** Nobody guessed "trait solving" before profiling. The usual advice (split the crate, cut the derives, change the linker) would have been effort spent on the wrong thing.
2. **Self-profiling with `args` turned a symptom into a cause.** The query totals said "trait solving". Grouping by predicate and by function body said "this one struct, proved `Send` over and over, by these methods".
3. **Type erasure and boxing are compile-time tools.** We usually think of `dyn` and `Box` as runtime choices. In async code, they're also the way to tell the compiler "you don't need to look inside this".
4. **Measure the advice, too.** We told ourselves, and this guide told you, that dropping `#[async_trait]` wouldn't help. A before/after profile of a single pull request, the last 62 uses, said it cut trait solving by 80%. Theories about the compiler's caches are worth exactly as much as the profile that backs them.

## Using cargo faster

Everything in this guide can be done by hand with Cargo, `rustc`, and the tools mentioned along the way. `cargo faster` automates the measuring. Each subcommand maps to a part of this guide:

- `cargo faster` runs every quick check at once and builds nothing. Start here.
- `cargo faster timings` reads a `--timings` report and finds [the critical path](#the-critical-path) and the crates that serialize the build.
- `cargo faster check-cost` does the [self-profiling](#self-profiling-one-crate) from the case study for you: it replays one crate's `rustc` command with `-Zself-profile`, then groups the [trait solving](#trait-solving-and-async-code) by predicate and by function body.
- `cargo faster setup` audits [linking](#linking) and [profile settings](#profile-settings).
- `cargo faster cache` checks [caching](#caching) and cold worktrees.
- `cargo faster machine` checks [the machine](#the-machine) before you believe any timing.
- `cargo faster deps` looks for [heavy features and duplicate versions](#dependencies-and-features).
- `cargo faster tests` checks how your [test suite](#test-suites) runs.
- `cargo faster rdeps` shows [rebuild fan-out](#rebuild-fan-out) and crates worth [splitting](#splitting-a-big-crate).

See [Using cargo faster](cargo-faster.md) for how to read each one's output, with real examples. If you work with a coding agent, the same workflow is written down as a skill in [`skills/debug-slow-rust-builds`](../skills/debug-slow-rust-builds/SKILL.md).
