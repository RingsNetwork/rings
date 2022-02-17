bns-node
===============

[![bns-node](https://github.com/BNSnet/bns-node/actions/workflows/bns-node.yml/badge.svg)](https://github.com/BNSnet/bns-node/actions/workflows/bns-node.yml)



### ICE Scheme:

1. Peer A:
{
	create offer,
	set it as local description
} -> Send Offer to Peer B

2. Peer B: {
  set receiveed offer as remote description
  create answer
  set it as local description
  Send Answer to Peer A
}

3. Peer A: {
   Set receiveed answer as remote description
}


### Keywords

* Candidate

	- A CANDIDATE is a transport address -- a combination of IP address and port for a particular transport protocol (with only UDP specified here).

	- If an agent is multihomed, it obtains a candidate from each IP address.

	- The agent uses STUN or TURN to obtain additional candidates. These come in two flavors: translated addresses on the public side of a NAT (SERVER REFLEXIVE CANDIDATES) and addresses on TURN servers (RELAYED CANDIDATES).


```
                 To Internet

                     |
                     |
                     |  /------------  Relayed
                 Y:y | /               Address
                 +--------+
                 |        |
                 |  TURN  |
                 | Server |
                 |        |
                 +--------+
                     |
                     |
                     | /------------  Server
              X1':x1'|/               Reflexive
               +------------+         Address
               |    NAT     |
               +------------+
                     |
                     | /------------  Local
                 X:x |/               Address
                 +--------+
                 |        |
                 | Agent  |
                 |        |
                 +--------+

                     Figure 2: Candidate Relationships

```

### Ref:

1. https://mac-blog.org.ua/webrtc-one-to-one-without-signaling-server

2. https://datatracker.ietf.org/doc/html/rfc5245

3. https://datatracker.ietf.org/doc/html/draft-ietf-rtcweb-ip-handling-01
