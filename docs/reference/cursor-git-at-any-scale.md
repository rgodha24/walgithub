# Git at any scale — Vicent Martí, Cursor, 2026-08-18

Context: **reference material, verbatim below this header** — the design walgit re-implements. Read it before
changing the WAL, publish, sync, or placement. walgit's architecture and deltas are in `AGENTS.md`; its acceptance
bar is `GOAL.md`.

Source: https://cursor.com/blog/git-at-any-scale (fetched 2026-08-20 with interactive diagrams reduced to text labels).

---

# Git at any scale


Vicent Martí · 27 min read

### Table of Contents


  * What's hard about Git?
  * Git without packfiles
  * GitHub and filesystems
  * Spokes and Consistency
  * Continuity
  * Origin



Hosting Git repositories at scale is a nightmare. When Linus Torvalds designed the first version of _the information manager from hell_ (that's actually the tagline for Git, [look it up](https://github.com/git/git/commit/e83c5163316)), he had a very specific use case in mind: his own. He wanted to replace BitKeeper, the distributed version control system that was being used to develop the Linux Kernel. Of course, the replacement had to be distributed too. The Kernel is an unusual software project; it is extremely decentralized, with many different maintainers for its many different subsystems. A distributed version control system is a natural fit for this workflow.

Twenty years later, Git has become an industry standard, but the truth is that its distributed nature is more of a hindrance than an advantage. The average open-source software project doesn't operate with a decentralized workflow. The average company definitely doesn't. They use the many advantages of the distributed model (such as being able to work offline, delay pushes, etc) but they very much rely on a centralized host. And hosting a Git repository, it turns out, is an incredibly hard thing to do.

## #What's hard about Git?

The challenge in hosting Git repositories at scale is inherent in the design of Git itself: a _distributed_ version control system means that all instances of a repository are identical. There's nothing special about the repository on a Git server that doesn't apply to a repository on a developer's laptop. Although at first it may appear that this makes hosting Git repositories straightforward (simply put an HTTP daemon in front of an on-disk copy of a repository and you've got a Git server going!), there are many hard scalability and reliability challenges that make this quite the opposite.

In a normal Git repository, your code and metadata (files, commits, trees) are compressed and stored in _packfiles_ — a simple binary serialization format which is convenient to deal with on a local machine, but not ideal to manage at scale on a server. Packfiles are the fundamental building block of Git storage _and_ Git networking. When you push or fetch data from a repository, it's transferred as a packfile.

This is how Git works by design, but it would be fair to think that it needn't be that way. After all, you do not control the Git client (at least not without annoying your users and adding a lot of friction), but within the walls of your own server, you can do _anything_ you want. Nothing ties you to using packfiles — Linus is not going to come over and check. The only restriction is that you do need to receive and send _packfiles_ over the network for all Git operations.

Over the years, companies that tried hosting Git repositories at scale noticed that this _packfile_ -based design was a major limitation on both availability and scalability. Packfiles are large binary files that must exist on a filesystem for Git to access them. The simple approach of having an HTTP server in front of a repository on disk has a very low ceiling. Ideally you'd want the repository to exist on many disks and many machines (this lets you run many Git operations in parallel, and keeps your repository available when a server crashes). But how do you do that?

There are broadly three possible approaches to accomplish this, in increasing order of complexity: distribute the filesystem, distribute the packfiles, or distribute Git itself.

## #Git without packfiles

Git is a content-addressable data store. All objects in a Git repository (blobs, trees, commits, etc) are keyed by the SHA-1 of their contents. This is something that intuitively maps very well to a distributed key-value store (the key is the SHA-1; the value is the actual object), and could provide a clean way to scale out the storage of a repository. But this actually doesn't work.

Here's the issue: the actual layout of a Git repository is a directed acyclic graph (DAG for short). You can look up any object via its SHA, but to perform even the most trivial operation in the repo, you must actually walk the DAG step by step.

[interactive diagram: walking a commit DAG through a key/value store, one round trip per object]

If you want to do an operation like listing the recent changes in a repository, you must process its commits. When you process a commit, you get a pointer to the root of its tree. From that tree, you get pointers to each file and each subtree. From the original commit, you get a pointer to its parent (the one that comes before it in the history). Crucially, at every step of this walk, you don't know the value of the next pointer until you fetch the previous one. If every fetch requires a round trip to a distributed store, things become very expensive very fast.

This approach to distributing Git at the object level has been tried before, many times, and it often fails at scale. The most promising implementation was attempted by my former mentor Shawn Pearce when he was working on the version control systems team at Google. His approach was [storing the objects in a distributed hash table](https://www.eclipse.org/lists/jgit-dev/msg01189.html). This was only possible thanks to JGit, a custom Git implementation in Java. Like any good ol' Java library, JGit provides enough interfaces and factories and interface factories to abstract all the details of a normal Git repository, including replacing its on-disk packfiles with a DHT. Although the system worked and results were good enough for normal Git operations, the limitations of the Git protocol (which again, require _packfiles_ to be sent over the network regardless of how you store data on the server) made the `git clone` performance bad enough to discard the design altogether.

## #GitHub and filesystems

A couple years after Git started to escape its Linux Kernel bubble, a scrappy startup was born in San Francisco. GitHub was founded in 2008 as a social coding platform with a very prescient tagline, "Git repository hosting: no longer a pain in the ass." I'm not joking here either, [look it up](https://web.archive.org/web/20080514210148/http://github.com/). There was, all the way back in 2008, a broad consensus that despite (or perhaps because of) Git's distributed design, you actually needed a centralized way to host Git repositories to make them user-friendly, and doing this was very painful. GitHub was set on changing that.

Its platform started as (and mostly still is) a Rails monolith. The very first versions were running off a single, albeit beefy, machine, with a Ruby server and copies of the repositories on disk next to it. Scaling a Rails app is easy: deploy more instances of it. But in this particular case, since Git is involved, they quickly ran into the recurring question we're trying to solve here: If the Rails app needs to access the Git repositories on disk, how do you deploy more copies of them?

Being a thrifty bunch of misfits, the early systems engineers at GitHub tried the simplest approach that could possibly fix their scaling problems. The thinking was that, if they focused on distributing the _filesystem_ (instead of packfiles, or Git itself), they could keep the Rails app unchanged and spend their time shipping more features for the ever-growing user base, instead of doing weird stuff with Git. Very pragmatic. It didn't work.

The team attempted many approaches to a distributed filesystem for Git data: the most obvious one, using NFS to store all repositories on a centralized server, was quickly discarded. The default implementation of Git makes a lot of assumptions about filesystem semantics (locking, tearing, reading, syncing...) that ensure decent performance on the local filesystem of a slow developer laptop, but pay no attention to how they behave over a networked filesystem. It was slow, and it was buggy.

Further attempts were made with (frankly, in retrospect, horrific) technologies that replicated the filesystem at the block level. A short-lived deployment with [GFS](https://en.wikipedia.org/wiki/GFS2). A longer-lived deployment based on [DRBD](https://en.wikipedia.org/wiki/DRBD). They all hit a wall. They were _terrible_ to operate day to day, and they didn't make up for it with good performance. It all boils down to the design of _packfiles_ on disk.

We've already seen how Git's graph-like data structures make round-trips prohibitively expensive. Unfortunately, a very similar principle also applies to the underlying data on-disk. There is no correlation between the layout of objects in the DAG and the way they're placed in a _packfile_. The key heuristic used when generating _packfiles_ is minimizing their size; objects are placed randomly throughout the pack, they are compressed, and crucially they're rarely stored whole. Most objects are stored as a delta on top of another object in the same packfile. Reading an individual object, after following the many logical hops in the graph data structure, also involves following physical hops in the on-disk format.

[interactive diagram: commit DAG → packfile layout, objects stored randomly and as deltas]

This kind of random walk across gigabytes of data, which must happen for every single Git operation performed on a repository, just doesn't play well with a networked filesystem (whether it replicates at the file or at the block level). The only way this works without slowing down to a crawl is if you can cache the whole file locally. But with hundreds of thousands of repositories in the same filesystem, caching is not an option.

Eventually, the systems engineers at GitHub bit the bullet and gave up distributing the filesystem. They started developing an RPC system so that repositories could live on dedicated fileservers, and updated the Rails app to do all operations remotely. This provided a good chunk of horizontal scalability, but didn't fix their availability, nor the performance for the busiest repositories. After all, every repository was still stored only on a single machine.

## #Spokes and Consistency

Spokes was originally developed at GitHub around 2013, and it has since become an industry standard. Most Git hosting services use a variant of the Spokes approach (application-level replication for Git repositories) in their architecture. The main reason Spokes has worked well for many years is that it made three fundamental choices that, over time, have been proven to be optimal:

  1. It doesn't distribute Git itself; it works at the packfile level.
  2. It stores all data as actual Git repositories on local NVMe disks.
  3. It replicates the Git data, but keeps all copies consistently in sync.



Because of the random read patterns across _packfiles_ we've just discussed, storing plain Git repositories on NVMe drives is basically a requirement to ensure all basic Git operations remain fast. They also keep clones efficient because you don't have to transform the data into what the Git client expects. They also let you focus on building a product on top of Git, as opposed to maintaining a fork of Git yourself that can operate on your weird repositories.

Keeping all the copies of the data consistently in sync is also, crucially, very good. This is something you find out the hard way, but the Git client _really_ doesn't play well with eventual consistency. If your local Git client pushes a commit and then fails to read it immediately after a fetch, that's bad news. Git finds that very confusing. If you run your CI pipeline across a hundred runners and three of them don't find the commit they're supposed to test after cloning your repository, that's bad news. It's also a very poor user experience.

Working with an eventually consistent view of a Git repository has a lot of sharp edges, whether it's on the client or in the backend. Hence, Spokes pays a very high complexity cost to ensure the system is always fully consistent. Let's see exactly what this means.

Spokes is a _consensus-based_ distributed system. It works by storing several copies of your Git repository on different servers. Whenever you push new data, an orchestrator fans out your push so that every instance of your repository receives a copy. The "fan-out" is synchronized with a classic consensus algorithm called [3PC (three-phase commit)](https://en.wikipedia.org/wiki/Three-phase_commit_protocol) so that a push is only accepted if a majority of the nodes acknowledge it.

QUORUM · 4/5tx #42RESTORE ALLCOORDINATORcollecting votesPARTICIPANT 1waitingPARTICIPANT 2waitingPARTICIPANT 3offlinePARTICIPANT 4waitingPARTICIPANT 5waiting

1 · VOTING2 · PRE-COMMIT3 · DO COMMIT

Three-phase commit is not related to Git at all. It’s a consensus algorithm that ensures that all nodes on a system agree to either commit or rollback a transaction; it does so in three round-trips. It is very similar to a Two-phase commit, but it introduces an extra “pre-commit” phase so that the system can recover if the coordinator goes offline in the middle of a transaction.

Before we can talk more about the way Spokes uses 3PC, we need to understand how a Git push works. A Git push has two components: a _packfile_ and a _reference transaction_. The packfile, which we've already talked about, contains the objects you're pushing to the repository (blobs, trees, and commits with your changes). The transaction is what actually publishes your changes to the repository by updating one or more references (e.g. the branch you're working on) to point to the commits you've just pushed.

This separation is very convenient here, because a pushed commit is not visible ("reachable" in Git parlance) until the reference that points to it has been updated. This means we can implement consensus for our pushes by fanning out the packfiles to all hosts simultaneously (we don't need to synchronize here) _and then_ doing three-phase commit with the reference transaction, which is much smaller and faster to synchronize than the packfile. Git itself has support for preparing reference transactions: it can acquire a lock on the reference, verify that the existing value is what's expected, and then hold the lock until it receives a commit or an abort command for the transaction.

Playback speed**0.010x**

Replicas**5**

One-way latency**20ms**

elapsed 0msSPOKEScoordinatorneeds 5 / 5 acks

UPLOADPREPARELOCKCOMMIT

Spokes distributes packs and then performs a three-phase commit for each push’s transaction. You can increase the number of replicas and the latency in this simulator to see how it affects commit throughput.

With this design, we ensure that every push is fully synchronized across all the replicas. Reads (fetches, clones) can then be safely routed to _any single replica_ , because every replica is always up to date.

This is essentially how Spokes works, and it has been working quite well for the past 13 years. Of course, Spokes is not perfect — no system is. In 2026, the way people use Git repositories has changed drastically, and we have learned many important lessons about building distributed systems along the way. Time and experience have shown which of Spokes's choices turned out to be optimal, and which did not.

One flaw that has turned out to be critical is the **constrained horizontal scalability of 3PC**. When Spokes was initially released, three replicas per repository was the sweet spot. You could serve your average repository from three copies with capacity to spare, with enough redundancy to keep accepting pushes even if one machine went down.

In 2026, things look very different. The average repository for an enterprise company is now a massive monorepo. Three replicas are not enough to serve the traffic for such repos, particularly when it comes to CI. Of course, nothing stops Spokes from running with more than three replicas, except the dreaded [tail at scale](https://cacm.acm.org/research/the-tail-at-scale/). Three-phase commit maps very elegantly to the Git transaction model, but as a consensus algorithm, it has fundamental limitations: the latency of every step is bound by the slowest of all the servers in the cluster. The more replicas you add to a cluster, the worse push throughput gets.

This scalability constraint also applies the other way. When agents work with Git repositories at scale, they often operate outside of a monorepo by creating vast numbers of small repositories, many of them throwaway, and most of them barely touched. Spokes struggles here because it still requires three replicas for every one of these repositories. Three mostly idle replicas, which cannot be trimmed down because then the system wouldn't be fully consistent and data loss would be possible. With three-phase commit, the floor is always too high, and the ceiling too low.

Another flaw, impossible to see up front, but painfully obvious after having suffered through it, is that Spokes can be _rough_ to operate at scale. Because the repositories on disk are always the source of truth for consensus, every copy of every repository is _very important_. You have to treat repositories as _pets, not cattle_.

This means, for starters, that you need to know exactly where every repository is. This adds a dependency (and a potential availability issue) on an external database that must keep a very large routing table mapping every repository to every machine where it's replicated. Every repository must also be checksummed, and its checksums constantly updated in that table, to ensure the repository remains valid on disk. As soon as something bad happens to the repository (and trust me, bad things happen all the time — Git can be very finicky in practice), you must detect it and schedule a repair job to bring it back to a healthy state. And you must do it very quickly! Because, again, the repositories on disk are the source of truth. A corrupted copy is as bad as a missing one. If two of the three copies are corrupt, the system can no longer accept pushes: there's no quorum.

## #Continuity

Continuity is the Git storage system we've developed at Cursor, with a very clear approach: learning from everything that Spokes did well, and fixing the things that, after many years, we now know are problems.

_Continuity_ is a simple system (a system cannot be easy to operate if it is not simple). The core primitive behind it is a write-ahead log, which we store in S3-compatible object storage. In production, we run directly on S3, but we designed it so it can be deployed on any cloud.

When a repository receives a push, we store the push as a WAL entry in S3. **We never acknowledge a push until it has been fully persisted.** Each push is stored as a separate object; we write the pushed packfile to disk and upload it to S3 simultaneously. Uploading a WAL entry, however, does not publish it. A push is only visible once we successfully prepare its reference transaction on a local copy of the repository and record a pointer to the WAL entry in the WAL index file, which is its own object in the store. **This forces all pushes to be linearizable.**

S3 · OBJECT STORE#1.wal#2.wal#19e37.wal#23c6e.walgitwal.pbetag e2GIT CLIENTgit pushWALGITreceivingBARE REPOidle

PUSHINDEXUPLOADGETLOCKPUTREF TXN

◀BackPauseNext▶

We try _not_ to do one single S3 write per push, because in busy repositories, this puts a hard cap on push throughput based on the latency of the S3 PUT operation. With a carefully tuned batching implementation, and with the only requirement of having to synchronize the reference transaction with a single local repository instead of a quorum of replicas, we have a system that can ingest pushes as fast as our disk allows.

The local copy of the repository is, of course, a normal Git repository stored on a very fast NVMe drive. We do the same thing that Spokes does because I think Spokes got that exactly right. It allows us to reuse all the amazing OSS work of the Git community, including the upstream Git client and its many performance optimizations. It lets us focus on shipping new features, instead of doing weird stuff with Git.

#### #Consensus

We've seen that one thing that makes a Spokes cluster hard to operate is that it's very important to keep track of the location of every repository on each server. _Continuity_ does this very differently. Where does every repository live? The answer is "anywhere". It doesn't matter! We treat repositories like a warm cache on disk, but the source of truth is always the write-ahead log in S3. The system is stateless, and there are no routing tables (and no relational database to operate — hashtag blessed). If a repository is missing from the local disk when accessed on a host, we just materialize it from the WAL. We can do this very efficiently, but of course we don't want to do this _all the time_ , because it'd be wasteful. In production, we use [rendezvous hashing](https://en.wikipedia.org/wiki/Rendezvous_hashing) to map a repository ID to the list of nodes where we expect it to be. All the state we require to route repositories is the repository ID and the current set of healthy nodes in a cluster. But if this state gets out of sync (e.g., a node becomes unhealthy), that's perfectly fine too. We'll just materialize the repository on whichever node comes next.

What about consensus? Elections? Which server is the primary for a given repository? It also doesn't matter! There's no state and no consensus here. Any server can be the primary. All updates to the write-ahead log are synchronized with an atomic compare-and-swap (CAS) operation on S3, so it's always safe for any instance of a repository to receive a push. Again, just like with routing, letting an arbitrary server act as the primary isn't the most efficient thing (it leads to CAS retries, which can delay pushes), so in practice we always choose the same server as the primary, the first one in the ranked list from rendezvous hashing. But in the corner cases — when there's a deploy, a failover, a network blip — we just don't care exactly which server is the primary. The system is designed to always be correct when degraded, and always fast when healthy.

S3 · OBJECT STORE4b1e#414b1e.walgitwal.pbetag e0WALGIT Aupload pack#414b1e.wal#429f4d.walgitwal.pbWALGIT Bupload pack#414b1e.wal#42b7e3.walgitwal.pb

UPLOADRACECONFLICTREFETCHREBASERETRYDONE

◀BackPauseNext▶

#### #Replication

Having a write-ahead log in S3 opens a world of possibilities when it comes to scale. We can have _literally_ any number of replicas, because the scalability of S3 is unmatched and all the replicas catch up directly from there. We perform optimistic replication by sending gossip UDP packets around our cluster. The packets contain all the required metadata for each replica to catch up directly from S3 after every push. "That is insane," I hear you mumble from behind your screen across time and space. "UDP is not a reliable transport." Of course it isn't. Nothing is reliable in a distributed system! The wire is not reliable, the routing is not reliable, and the topology is not reliable either. But it's OK: it doesn't matter. Each replica knows the ETag of the last version of the WAL index it's caught up with. When you perform a read operation on a replica, we do a conditional GET to S3 with the ETag we expect. A 304 response with no body (conveniently, an almost instant operation — less than 10ms on average because it's a metadata-only S3 operation) means we're up to date and we can serve the fetch or the clone straight away. A 200 response comes with the newest version of the WAL index, which we use to catch up before serving the read.

S3 · OBJECT STORE#40.wal#41.wal#40b8ab.wal#4156e2.walgitwal.pbetag e1GIT CLIENTgit pushGIT CLIENTgit fetchWALGIT · PRIMARYreceive-packWALGIT · REPLICAcurrent · e1#40b8ab.wal#4156e2.walgitwal.pbetag e1

Drop the UDP gossip datagram

PUSHCOMMITGOSSIPREPLICATEFETCHGET · 304SERVE

◀BackPauseNext▶

It doesn't matter if the replication UDP packet is lost, or if it arrives at the wrong server because the topology shifted. All reads on all replicas are fully consistent, because they're verified against the source of truth, which is S3. The system is designed to always be correct when degraded, and always fast when healthy.

The implications of this are twofold. First, because the system is always consistent, building infrastructure on top of it is trivial. We (our agents, our web interface, our clients) always see a globally consistent view of the repository. And because the system scales in _both_ directions, every repository gets just the right number of replicas. A large monorepo can be deployed across hundreds of replicas to serve all the load from its CI jobs. Millions of tiny repositories created by agents can be served with one replica each; we don't need more than one to ensure availability, because S3 is the source of truth. In fact, an idle repository doesn't even need that: when a replica hasn't received traffic for a while, we garbage collect it from the node's disk, and simply materialize it again from the WAL the next time a fetch comes in.

S3 · OBJECT STORE

#### #Compaction

Write-ahead logs require periodic compaction. You cannot let the log grow unbounded: a full restore replays every entry, so the more entries, the more expensive it becomes.

Coincidentally, a normal Git repository _also_ requires periodic compaction, even though Git is not based on a WAL. We've seen that the fundamental unit of storage in a Git repository is the _packfile_. Each time you push to a remote copy of a repository, or fetch into your local copy, you create a new packfile. This doesn't scale indefinitely: each packfile has its own attached index, which allows Git to efficiently look up the objects it contains, but this lookup is only efficient on a per-_packfile_ basis. If you're looking for a specific object, and your repository has 100 packfiles, you'll need to open the index for each one of them and look up the object until you find it in one of the packfiles. An efficient operation is not efficient if it must be performed hundreds or thousands of times.

Modern Git has gotten very good at working around this; it now supports multi-pack indexes and incremental geometric compaction. But eventually you must bite the bullet and repack your Git repository on disk. Historically, this has been a constant availability issue for systems like Spokes, because repacking is a very CPU-heavy operation, even when done incrementally, and it must be performed on all the replicas of the system. Accidentally triggering a maintenance operation on two or more Spokes nodes for the same repository will easily cause the repository to fail over.

Here, we amortize the cost of compaction. Only the primary does compactions, and the result of the compaction applies to both the on-disk repository _and_ the WAL. Since all replicas follow the WAL, they also follow the compaction events. Replicas don't repack; they simply download the already-compacted packs from S3, trading bandwidth for CPU.

WALGIT · LOCAL PACKSgeometric compaction0ec8f61e8f28979dbe0e6db3COMPACTION FRONTIERPACKS ARRIVES3 · SOURCE OF TRUTH5 packs#126db3.wal#40be0e.wal#970ec8.wal#98f61e.wal#998f28.walgitwal.pb

#### #Scale

Replication and compaction are the two key factors that determine how well a Git storage system behaves under load. As we’ve just seen, they’re intrinsically linked: the more pushes per second a repository ingests, the more read performance degrades, because the packfiles of every push must be compacted for Git operations to remain efficient. If you replicate these pushes, the compaction must be either replicated or performed independently on each replica.

_Continuity’s_ WAL-first design offers **fully consistent horizontal scalability** : you can deploy an arbitrary number of replicas, and the throughput for read-only Git operations grows linearly with them. Because all replicas in the cluster are fully consistent, this allows us to scale the Git protocol (clones, fetches) _and_ all the RPC operations that Origin performs on top of repositories (web UI interactions, the REST API, all our agentic interfaces, etc.)

We have run synthetic stress tests with up to 100 replicas and seen consistent linear scaling for reads, without any regressions in push throughput.

The push throughput of a cluster depends on the latency at which we can update our WAL on S3. Using S3 Standard, we can sustain up to 120 pushes/s while compacting and replicating the compacted data to all other nodes. We have also deployed high-performance clusters on S3 Express One Zone, which has much lower latency for PUT operations. There, we can ingest more than 300 pushes/s, and we are effectively bottlenecked by the speed at which Git can compact the on-disk data. We’re working on innovative ways to lay out this data on disk to reduce the impact of compaction: our goal is to continue optimizing the speed at which a Git repository can ingest code without relaxing our hard durability and consistency guarantees.

  * S3 Standard
  * S3 Express One Zone



Push/Clone throughput for `everysphere`, Cursor’s monorepo.  
All pushes are linearizable and persisted to external storage before acknowledgement.  
All clones are fully consistent.

  * S3 Standard
  * S3 Express One Zone



Push/Clone throughput for `everysphere`, Cursor’s monorepo.  
All pushes are linearizable and persisted to external storage before acknowledgement.  
All clones are fully consistent.

  * S3 Standard
  * S3 Express One Zone



Push/Clone throughput for `everysphere`, Cursor’s monorepo.  
All pushes are linearizable and persisted to external storage before acknowledgement.  
All clones are fully consistent.

#### #WAL as truth

S3 is a great piece of technology. The whole concept of blob storage that was pioneered with the S3 API has turned out to be a very powerful building block for large data storage systems, and this most definitely also applies to hosting Git repositories. The design presented here is novel on many ways, but it's not the first one to store packfiles as blobs. Azure DevOps (Microsoft's own competitor to Microsoft's own GitHub) has a very successful Git storage system that stores packfiles in blob storage and their references in a relational database (MS SQL Server). There are many trade-offs to a system like this. A relational database scales well with large reference transactions. But then you have to operate a relational database. We have a strong belief that the consistency of Git data is more important than any other consideration. This is what really tipped the scales for us into designing a WAL-based system that doesn't depend on external databases.

There are many things that can go wrong with a Git repository in production. Data corruption at rest, bugs during repacking, races during pushes. It's one big collection of corner cases. Most of these have been ironed out in Git upstream. But not all of them. No system is without bugs, not even those that are OSS and widely deployed. Our consistency model ensures that we keep track of every fundamental operation that happens to a repository. **We never acknowledge a push until it has been fully persisted to the WAL. We linearize _all_ pushes. Every view of every repository we access is always fully consistent.** Since every push is in the WAL, we can look at every state a repository has ever been in. We have full provenance data for all pushes, and also for all repacks. We can rewind and fast-forward every replica. We don't have to synchronize any state with any external database, whether it's a database that only stores references, or a database that stores all object data. When (not if) we hit a bug in Git, we can pinpoint exactly what happened and revert it. And besides the bugs that already exist in Git, we introduce very few new ones, because throughout all this, all Git operations are performed on a normal Git repository on disk, using off-the-shelf tooling.

## #Origin

We are acutely aware of how important it is to host somebody's source code. I think everybody who reads and understands this blog post is just as aware of it. A company can grind to a halt if its developers cannot push or pull from its Git repositories. The productivity cost of five minutes of downtime in your CI system is hard to quantify in dollars, but it is, by any measure, a humongous amount.

Agents have fundamentally changed the way we work with software, and in many ways they've made this situation worse. More code, more PRs, more CI runs. Version control is at the core of all of this, and it is possibly the hardest thing to change overnight.

We've faced these difficulties internally at Cursor for many months now, and we've put considerable thought and care into building a platform that solves them for us and that can hopefully solve them for our customers too. Our focus right now is on providing the smoothest possible off-ramp into more reliability, more performance and more scale, and making the migration as painless as possible.

Origin is not an experiment; it is the result of many decades of experience building these same systems, from people who deeply understand the magnitude of the challenges involved. We have an engineering and operational philosophy that has been proven to work, and a strong commitment to continue evolving it as the landscape of version control evolves.

We're hoping you'll place your trust in us and our platform.
