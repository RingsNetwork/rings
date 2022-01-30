# bns-node

[![bns-node](https://github.com/BNSnet/bns-node/actions/workflows/bns-node.yml/badge.svg)](https://github.com/BNSnet/bns-node/actions/workflows/bns-node.yml)


ICE Scheme:

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


=================

PEER A                                      PEER B
   |                                          |
a). create offer
b). set it as local description
c). send it to B

                                     a). set receiveed offer as remote description
									 b). create answer
									 c). set it as local description
									 d). send it to A
set it as remote description




Ref: https://mac-blog.org.ua/webrtc-one-to-one-without-signaling-server
