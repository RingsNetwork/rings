// twist from https://github.com/jwasinger/mimc-merkle-proof/blob/master/circuit/merkle_tree.circom

include "node_modules/circomlib/circuits/mimcsponge.circom";


// Computes MiMC([left, right])
template HashLeftRight() {
  signal input left;
  signal input right;
  signal output hash;

  component hasher = MiMCSponge(2, 220, 1);
  hasher.ins[0] <== left;
  hasher.ins[1] <== right;
  hasher.k <== 0;
  hash <== hasher.outs[0];
}


// if s == 0 returns [in[0], in[1]]
// if s == 1 returns [in[1], in[0]]
template DualMux() {
  signal input in[2];
  signal input s;
  signal output out[2];

  s * (1 - s) === 0;
  out[0] <== (in[1] - in[0])*s + in[0];
  out[1] <== (in[0] - in[1])*s + in[1];
}

template MerkleTreeLocalCheck() {
  /// current leaf of tree
  signal input leaf;
  // input for each recursion
  signal input path[2];
  signal output output_root;

  component selector;
  component nextHash;

  selector = DualMux();
  selector.in[0] <== leaf;
  selector.in[1] <== path[0];
  selector.s <== path[1];

  nextHash = HashLeftRight();
  nextHash.left <== selector.out[0];
  nextHash.right <== selector.out[1];
  output_root <== nextHash.hash;
}

component main { public [leaf]} = MerkleTreeLocalCheck();
