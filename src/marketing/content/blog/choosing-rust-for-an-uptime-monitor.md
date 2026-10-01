+++
title = "Why I chose Rust over Go for an uptime monitor"
date = "2026-07-18"
updated = "2026-10-01"
slug = "choosing-rust-for-an-uptime-monitor"
excerpt = "Go is the usual pick for a service like this. I chose Rust for one reason: a monitor sells clean timing, and its own runtime must not add random delay."
tags = ["rust", "go", "monitoring", "devops"]
draft = false

[[faqs]]
q = "Is Rust better than Go for an uptime monitor?"
a = "Rust has no garbage collector. For this job that matters, because the prober does not add random pauses to the latency it reports, and the compiler catches data races before they ship. Go is still an excellent choice for most network services, and it is faster to learn and build."

[[faqs]]
q = "Does Go's garbage collector really affect latency?"
a = "Go has a garbage collector. It is fast and most apps never feel it, but at high concurrency its work can land inside the millisecond timings a monitor reports and lift high percentiles like p99. A runtime with no garbage collector removes that source of noise."

[[faqs]]
q = "How much memory does the Rust monitor use?"
a = "In one run, a single machine held 50,000 checks in flight and peaked at 933 MiB of memory. A live server uses around 42 MiB while running its monitors. These are laptop load-test numbers for catching slowdowns between versions, not for production capacity planning."

[[faqs]]
q = "Is Rust worth the slower development for a solo developer?"
a = "Rust asks more from you first. For a narrow, timing-sensitive service run without a team, the compiler catching data races and memory bugs pays back the slower build. For a general web app on a deadline, Go or another language may be the better trade."
+++

I build Uptimepage, an [open-source uptime monitor](/open-source-uptime-monitoring) and status page written in Rust. People ask why Rust and not Go, since Go is the usual pick for this kind of network service. Here is the honest answer. It is not that Rust wins everywhere. It is that one part of this job made the choice for me.

The product is one promise: tell you fast and honestly when your site is slow or down. The numbers I show you, like your p99 response time, have to be clean. If my own code adds random delay, I blur the exact signal you pay for. So the runtime under the prober matters more here than it would for a normal app.

## Garbage collection shows up in the tail

Go has a garbage collector. It is fast, and most apps never feel it. But it still has to do work to free memory, and that work can land inside the millisecond timings I report. Run tens of thousands of checks at once and a pause at the wrong moment lifts a p99 number. At that point I am measuring my own runtime, not your server.

Rust has no garbage collector. Memory is freed at a point I can see in the code. There is no background pause I did not write. For a tool that sells timing, that control is worth the extra work Rust asks for.

## What that looks like in memory

Here are some real numbers, with a warning attached. They come from a load test on a developer laptop, not a production server. I use them to catch a slowdown between two versions of my code, not to plan capacity. A real server does better, a small box does worse. Treat them as a floor.

In one run, a single machine held 50,000 checks in flight and peaked at 933 MiB of memory. That is under one gigabyte for fifty thousand live checks. My running server uses about 42 MiB while watching its monitors, and it sits quiet when there is nothing to do. That kind of density is normal for a service with no garbage collector, and it means one small box covers a lot of monitors. I go deeper on the prober and the throughput numbers in [the build story](/blog/building-an-uptime-monitor-in-rust).

## The bug that never compiles

Speed is only half of it. The other half is a class of bug that Rust refuses to build.

Picture many workers writing to one shared map of results at the same time. In Go this compiles and runs. Sometimes it is fine. Sometimes two goroutines write at once, you get corrupt data or a crash, and it only happens under load, which is the worst time to find out:

```go
// compiles fine, races at runtime
results := map[string]int{}
for _, check := range checks {
    go func(c Check) {
        results[c.ID] = c.Latency // data race
    }(check)
}
```

Go gives you good tools for this. There is a race detector, a sync.Mutex, and channels. But remembering to reach for them is on you.

In Rust the same concurrent write does not compile. The compiler stops you until the shared map is wrapped in a lock:

```rust
// will not compile unless the shared map is a Mutex
let results = Mutex::new(HashMap::new());
std::thread::scope(|s| {
    for check in checks {
        let results = &results;
        s.spawn(move || {
            // each worker locks only for its own insert
            results.lock().unwrap().insert(check.id, check.latency);
        });
    }
});
```

Each worker holds the lock only for its own insert, so the writes stay safe without blocking the others for long. For code that runs day and night across many machines, "the compiler will not let you ship the race" removes a whole set of late-night bugs before they exist.

## Where Go is the better pick

None of this makes Go a bad choice. Often it is the better one. Go is faster to learn. It builds in seconds. Its standard library for network services is excellent, and a new engineer can be useful in days. If I were building a normal web service, or something I had to ship this week, Go would be on the table and might win.

Rust asks more from you first. The compiler argues with you. The build is slower. You spend time on things Go would just handle. I take that trade because this job is narrow and it rewards tight control over memory and timing.

## The compiler as a safety net

Heavy code review is how large teams catch data races and memory bugs. Rust gives you a lot of that for free. The compiler turns those mistakes into build errors, so they never reach production and never wake anyone up. Every change gets a strong, automatic check before it ships, which is a big part of why I trust the service to run unattended.

It is also why Uptimepage is open source and self-hostable. You can read the code, run it on your own hardware, and export your data whenever you want, so you are never locked into a single vendor. You can start from [GitHub](https://github.com/uptimepage/uptimepage).

## The honest version

The honest version is not "Rust beats Go." It is "for a tool that lives or dies by timing and runs at high concurrency, Rust fit better." Pick the language for the job in front of you. Mine happened to be a job that Rust is very good at.

If you want to see what that decision produced rather than the reasoning behind it, the [live architecture map](/architecture) traces a request and a check through every hop of the running system.

## Common questions

<details class="mk-faq">
<summary>Is Rust better than Go for an uptime monitor?</summary>
<div class="mk-faq__body">

Rust has no garbage collector. For this job that matters, because the prober does not add random pauses to the latency it reports, and the compiler catches data races before they ship. Go is still an excellent choice for most network services, and it is faster to learn and build.

</div>
</details>

<details class="mk-faq">
<summary>Does Go's garbage collector really affect latency?</summary>
<div class="mk-faq__body">

Go has a garbage collector. It is fast and most apps never feel it, but at high concurrency its work can land inside the millisecond timings a monitor reports and lift high percentiles like p99. A runtime with no garbage collector removes that source of noise.

</div>
</details>

<details class="mk-faq">
<summary>How much memory does the Rust monitor use?</summary>
<div class="mk-faq__body">

In one run, a single machine held 50,000 checks in flight and peaked at 933 MiB of memory. A live server uses around 42 MiB while running its monitors. These are laptop load-test numbers for catching slowdowns between versions, not for production capacity planning.

</div>
</details>

<details class="mk-faq">
<summary>Is Rust worth the slower development for a solo developer?</summary>
<div class="mk-faq__body">

Rust asks more from you first. For a narrow, timing-sensitive service run without a team, the compiler catching data races and memory bugs pays back the slower build. For a general web app on a deadline, Go or another language may be the better trade.

</div>
</details>
