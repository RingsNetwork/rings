rings-node
===============

[![rings-node](https://github.com/RingsNetwork/rings-node/actions/workflows/rings-node.yml/badge.svg)](https://github.com/RingsNetwork/bns-node/actions/workflows/rings-node.yml)



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

* Channel

In the WebRTC framework, communication between the parties consists of media (for example, audio and video) and non-media data.

Non-media data is handled by using the Stream Control Transmission Protocol (SCTP) [RFC4960] encapsulated in DTLS.

```
                               +----------+
                               |   SCTP   |
                               +----------+
                               |   DTLS   |
                               +----------+
                               | ICE/UDP  |
                               +----------+

```

The encapsulation of SCTP over DTLS (see [RFC8261]) over ICE/UDP (see [RFC8445]) provides a NAT traversal solution together with confidentiality, source authentication, and integrity-protected transfers.


 The layering of protocols for WebRTC is shown as:

```
                                 +------+------+------+
                                 | DCEP | UTF-8|Binary|
                                 |      | Data | Data |
                                 +------+------+------+
                                 |        SCTP        |
                   +----------------------------------+
                   | STUN | SRTP |        DTLS        |
                   +----------------------------------+
                   |                ICE               |
                   +----------------------------------+
                   | UDP1 | UDP2 | UDP3 | ...         |
                   +----------------------------------+
```


### Architecture


```
+-----------------------------------------------------------------------------------------+
|                                         BNS                                             |
+-----------------------------------------------------------------------------------------+
|   Encrypted IM / Secret Sharing Storage / Distributed Content / Secret Data Exchange    |
+-----------------------------------------------------------------------------------------+
|                  SSSS                  |  Perdson Commitment/zkPod/ Secret Sharing      |
+-----------------------------------------------------------------------------------------+
|                     |       dDNS       |                 Sigma Protocol                 |
+      K-V Storage    +------------------+------------------------------------------------+
|                     |  Peer LOOKUP     |          MSRP        |  End-to-End Encryption  |
+----------------------------------------+------------------------------------------------+
|            Peer-to-Peer Network        |                                                |
|----------------------------------------+               DID / Resource ID                |
|                 Chord DHT              |                                                |
+----------------------------------------+------------------------------------------------+
|                Trickle SDP             |        ElGamal        | Persistence Storage    |
+----------------------------------------+-----------------------+------------------------+
|            STUN  | SDP  | ICE          |  Crosschain Binding   | Smart Contract Binding |
+----------------------------------------+------------------------------------------------+
|             SCTP | UDP | TCP           |             Finate Pubkey Group                |
+----------------------------------------+------------------------------------------------+
|   WASM Runtime    |                    |                                                |
+-------------------|  Operation System  |                   ECDSA                        |
|     Browser       |                    |                                                |
+-----------------------------------------------------------------------------------------+
```



### Ref:

1. https://datatracker.ietf.org/doc/html/rfc5245

2. https://datatracker.ietf.org/doc/html/draft-ietf-rtcweb-ip-handling-01

3. https://datatracker.ietf.org/doc/html/rfc8831

4. https://datatracker.ietf.org/doc/html/rfc8832
