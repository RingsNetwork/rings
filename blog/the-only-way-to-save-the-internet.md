![Illustration 1](/assets/blog/the-only-way-to-save-the-internet/figure-1.jpeg)

## Web2: The System is failing

Once a beacon of decentralized dreams, the Internet, as WWW creator Tim Berners-Lee pointed out[1], is showing signs of systemic failure. This isn’t something we can pin on governments or businesses going about their rightful activities; we need to look at the intrinsic issues of the Internet itself.

Enter the notorious “Waist of the Internet” problem[2]. This Hourglass model illustrates that even with decentralization at the application or protocol layers, the centralized nature of DNS/IP still poses a bottleneck. From its inception, the seeds of centralization and control were sown into the Internet’s very design.

This is precisely what’s choking the future potential of Web2 and even the emerging Web3.

## The Elephant in the Room: Is Web3 Working?

![Illustration 2](/assets/blog/the-only-way-to-save-the-internet/figure-2.jpeg)

Web3 represents a pivotal exploration as people recognize the potential downfall of Web2. It started as the Semantic Web, evolved into the decentralized web (dWeb), and now closely aligns with various blockchain technologies.

When discussing Web3, buzzwords like “decentralized,” “immutable,” “permissionless,” and “privacy” flood the conversation. Yet, the elephant in the room, the unsaid challenges behind these buzzwords, are often ignored.

Web3 is still, fundamentally, the Web. It can’t escape the “Waist of the Internet” dilemma, heavily relying on centralized infrastructures like IP, DNS, servers, and nodes. And these so-called web3 infrastructures are becoming increasingly centralized due to high costs and business interests, much like what we witnessed with Web2.

The Ethereum Foundation once ambitiously launched the Whisper project[3], a beacon of hope for Web3 to solve the “Waist of the Internet” problem, facilitating decentralized peer-to-peer communication. But now, its remnants lie in a GitHub repository with Ethereum developers conceding:

> “The rationale for archiving this project is that it is obvious that in its current implementation, Whisper will never scale beyond a couple hundred nodes. Further development has now been taken on by status.”

If foundational projects like Whisper hit a wall, then L1s, L2s, or even Lightning Networks are bound to grapple with the same curse. The spirit of the Internet and blockchain, the so-called decentralization, remains an elusive mirage.

## The Path to Redemption

Yet, I believe there’s a path to redemption, and it begins with tackling the “Waist of the Internet”. To remove this waist, we need to embrace a network where every service and node operates on a peer-to-peer, fully decentralized basis. An idealistic Internet where everyone is a client, without central servers. Services should run on every client, sharing and cooperating seamlessly — that would be the true essence of both the Internet and Web3.

However, this is no easy feat, with few viable solutions. Or perhaps there is only one: leveraging the client-side technology that everyone uses to access the Internet — the web browser. Browsers must connect directly, building a vast decentralized network that allows users to access services without centralized infrastructure.

This is a tall order, but thankfully, there’s one clear route to get there.

First, browsers need the ability to establish direct connections — pointing to WebRTC technology for peer-to-peer communication similar to TCP/IP.

Second, we want all services and complex logic to run directly in the browser, leading us to WebAssembly, which allows code written in any language (like Rust, Golang, C++) to run in the browser.

Then, we need a reliable service access and routing mechanism to replace the central DNS/IP issue at the “waist” — this points us towards DHT (Decentralized Hash Table) technology, useful for decentralized storage and node discovery.

Lastly, we want a network that can interlink service islands, using technologies like Bitcoin, Ethereum, Polkadot, or even Web Crypto for authentication. We desire direct communication between entities, which directs us to Abstract Accounts and DID technology.

The path is singular, as is the savior. This conviction is one reason why I’m fervently developing the Rings Network.

![Logo of Rings Network](/assets/blog/the-only-way-to-save-the-internet/figure-3.jpeg)

## The Future

So when I chat with fellow developers about my work, explaining that Rings Network is leveraging WebRTC for transport, compiling into Wasm to run in browsers, using the Chord algorithm for DHT, and supporting a wide range of elliptic curves for Abstract Accounts and DIDs.

They smile knowingly and say, “Ah, you’re building the future,” even without me having to explain what we’re doing in detail.

Because this is the only way.[4]

References:

[1] [https://www.theguardian.com/technology/2017/nov/15/tim-berners-lee-world-wide-web-net-neutrality](https://www.theguardian.com/technology/2017/nov/15/tim-berners-lee-world-wide-web-net-neutrality)

[2] [https://www.oilshell.org/blog/2022/02/diagrams.html](https://www.oilshell.org/blog/2022/02/diagrams.html) [3] [https://github.com/ethereum/whisper](https://github.com/ethereum/whisper)

[3] [https://github.com/ethereum/whisper](https://github.com/ethereum/whisper)

[4] [https://github.com/RingsNetwork/rings-node](https://github.com/RingsNetwork/rings-node)
