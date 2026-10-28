//! `homeport bench`: time-to-listing percentiles and read throughput.

use std::io::Write;
use std::time::{Duration, Instant};

use homeport_client::{Client, Error, ListResult};
use homeport_proto::{Item, Kind, Priority};

use crate::CliError;

#[derive(clap::Args, Debug)]
pub struct BenchArgs {
    /// Directories under `--under` to list.
    #[arg(long, default_value_t = 50)]
    dirs: usize,
    /// Repetitions of the warm and NotModified passes.
    #[arg(long, default_value_t = 10)]
    iters: usize,
    /// Parent of the benchmark directories.
    #[arg(long, default_value = "dev")]
    under: String,
    /// A large directory to measure separately.
    #[arg(long, default_value = "Downloads")]
    big: String,
    /// File to read for throughput; defaults to the largest file (≤ 1 GiB) in `--big`.
    #[arg(long)]
    read: Option<String>,
}

struct Stats(Vec<Duration>);

impl Stats {
    fn pct(&self, p: f64) -> f64 {
        let mut v = self.0.clone();
        v.sort_unstable();
        if v.is_empty() {
            return 0.0;
        }
        let i = ((v.len() as f64 - 1.0) * p).round() as usize;
        v[i].as_secs_f64() * 1000.0
    }
    fn line(&self, label: &str) -> String {
        format!(
            "{label:<34} n={:<5} p50 {:>7.2} ms  p95 {:>7.2} ms  p99 {:>7.2} ms  max {:>7.2} ms",
            self.0.len(),
            self.pct(0.5),
            self.pct(0.95),
            self.pct(0.99),
            self.pct(1.0)
        )
    }
}

async fn timed_list(c: &Client, id: &str, rev: Option<u64>) -> Result<(Duration, ListResult), Error> {
    let t = Instant::now();
    let r = c.list_dir(id, rev, Priority::Visible).await?;
    Ok((t.elapsed(), r))
}

fn items(r: &ListResult) -> &[Item] {
    match r {
        ListResult::Listing { items, .. } => items,
        ListResult::NotModified { .. } => &[],
    }
}

pub async fn run(c: &Client, a: BenchArgs, out: &mut impl Write) -> Result<(), CliError> {
    let mut pings = Stats(Vec::new());
    for _ in 0..20 {
        pings.0.push(c.ping().await?);
    }
    writeln!(out, "{}", pings.line("ping"))?;

    let parent = c.resolve_path(&a.under, Priority::Visible).await?;
    let (_, top) = timed_list(c, &parent.id, None).await?;
    let dirs: Vec<Item> =
        items(&top).iter().filter(|i| i.kind == Kind::Dir && !i.name.starts_with('.')).take(a.dirs).cloned().collect();

    let mut first = Stats(Vec::new());
    let mut revs = Vec::new();
    let mut entries = 0usize;
    for d in &dirs {
        let (t, r) = timed_list(c, &d.id, None).await?;
        entries += items(&r).len();
        first.0.push(t);
        revs.push((d.id.clone(), r.dir_rev()));
    }
    writeln!(out, "{} ({} entries total)", first.line("first listing (server cold?)"), entries)?;

    let mut warm = Stats(Vec::new());
    let mut nm = Stats(Vec::new());
    for _ in 0..a.iters {
        for (id, _) in &revs {
            warm.0.push(timed_list(c, id, None).await?.0);
        }
        for (id, rev) in &revs {
            let (t, r) = timed_list(c, id, Some(*rev)).await?;
            if !matches!(r, ListResult::NotModified { .. }) {
                writeln!(out, "note: {id} changed during the run")?;
            }
            nm.0.push(t);
        }
    }
    writeln!(out, "{}", warm.line("warm full listing"))?;
    writeln!(out, "{}", nm.line("warm NotModified"))?;

    let t = Instant::now();
    let many = c.list_many(revs.iter().map(|(id, rev)| (id.clone(), Some(*rev))).collect(), Priority::Prefetch).await?;
    writeln!(out, "list_many NotModified x{:<4}         {:>7.2} ms", many.len(), t.elapsed().as_secs_f64() * 1000.0)?;
    let t = Instant::now();
    let many = c.list_many(revs.iter().map(|(id, _)| (id.clone(), None)).collect(), Priority::Prefetch).await?;
    writeln!(out, "list_many full x{:<4}                {:>7.2} ms", many.len(), t.elapsed().as_secs_f64() * 1000.0)?;

    // Coalescing: identical concurrent listings share one request.
    if let Some((id, _)) = revs.first() {
        let t = Instant::now();
        let futs = (0..32).map(|_| c.list_dir(id, None, Priority::Visible));
        let n = futures::future::join_all(futs).await.into_iter().filter(Result::is_ok).count();
        writeln!(out, "32 identical concurrent listings    {:>7.2} ms ({n} ok)", t.elapsed().as_secs_f64() * 1000.0)?;
    }

    let big = c.resolve_path(&a.big, Priority::Visible).await?;
    let (t, r) = timed_list(c, &big.id, None).await?;
    let big_items: Vec<Item> = items(&r).to_vec();
    writeln!(
        out,
        "big dir {:?}: {} entries, first listing {:.2} ms",
        a.big,
        big_items.len(),
        t.as_secs_f64() * 1000.0
    )?;
    let mut bw = Stats(Vec::new());
    let mut bn = Stats(Vec::new());
    for _ in 0..a.iters.max(5) {
        bw.0.push(timed_list(c, &big.id, None).await?.0);
        bn.0.push(timed_list(c, &big.id, Some(r.dir_rev())).await?.0);
    }
    writeln!(out, "{}", bw.line("big dir warm full listing"))?;
    writeln!(out, "{}", bn.line("big dir NotModified"))?;

    // Small reads: time to first 4 KiB (what opening a small file costs).
    let small: Vec<&Item> = big_items.iter().filter(|i| i.kind == Kind::File && i.size >= 4096).take(20).collect();
    let mut sr = Stats(Vec::new());
    for i in &small {
        let t = Instant::now();
        c.read_range(&i.id, 0, 4096, false, Priority::UserOpen).await?;
        sr.0.push(t.elapsed());
    }
    writeln!(out, "{}", sr.line("read 4 KiB"))?;

    let target = match a.read {
        Some(p) if p.bytes().all(|b| b.is_ascii_digit()) => c.stat(&p, Priority::Visible).await?,
        Some(p) => c.resolve_path(&p, Priority::Visible).await?,
        None => big_items
            .iter()
            .filter(|i| i.kind == Kind::File && i.size <= 1 << 30)
            .max_by_key(|i| i.size)
            .cloned()
            .ok_or_else(|| CliError::Usage("no file to read".into()))?,
    };
    for compress in [false, true] {
        let t = Instant::now();
        let mut s = c.read_stream(&target.id, 0, 0, compress, Priority::UserOpen).await?;
        let mut n = 0u64;
        while let Some((_, chunk)) = s.next_chunk().await? {
            n += chunk.len() as u64;
        }
        let secs = t.elapsed().as_secs_f64();
        writeln!(
            out,
            "read {:?} ({:.1} MiB, compress={compress}): {:.2} s, {:.1} MiB/s, wire {:.1} MiB",
            target.name,
            n as f64 / 1048576.0,
            secs,
            n as f64 / 1048576.0 / secs,
            s.wire_bytes() as f64 / 1048576.0
        )?;
    }
    // Priorities: urgent small requests while a background bulk read runs,
    // then cancel the bulk read by dropping it.
    let bulk_client = c.clone();
    let bulk_id = target.id.clone();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
    let bulk = tokio::spawn(async move {
        let mut s = bulk_client.read_stream(&bulk_id, 0, 0, false, Priority::Background).await?;
        let mut n = 0u64;
        loop {
            tokio::select! {
                _ = &mut rx => return Ok::<_, Error>(n),
                chunk = s.next_chunk() => match chunk? {
                    Some((_, b)) => n += b.len() as u64,
                    None => return Ok(n),
                },
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut busy_read = Stats(Vec::new());
    let mut busy_list = Stats(Vec::new());
    for i in small.iter().cycle().take(40) {
        let t = Instant::now();
        c.read_range(&i.id, 0, 4096, false, Priority::UserOpen).await?;
        busy_read.0.push(t.elapsed());
        let (id, rev) = &revs[busy_list.0.len() % revs.len().max(1)];
        busy_list.0.push(timed_list(c, id, Some(*rev)).await?.0);
    }
    let _ = tx.send(());
    let moved = bulk.await.map_err(|e| CliError::Usage(e.to_string()))??;
    writeln!(out, "{}", busy_read.line("read 4 KiB during bulk read (P0)"))?;
    writeln!(out, "{}", busy_list.line("NotModified during bulk read (P1)"))?;
    writeln!(out, "bulk read cancelled after {:.1} MiB", moved as f64 / 1048576.0)?;

    let l = c.link();
    writeln!(out, "link {} rtt {:?} loss {:.2}%", l.state, l.rtt, l.loss * 100.0)?;
    Ok(())
}
