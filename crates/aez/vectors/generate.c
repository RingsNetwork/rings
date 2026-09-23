/*
 * AEZ v5 known-answer vectors for rings-aez, emitted by the official reference code
 * (https://www.cs.ucdavis.edu/~rogaway/aez/code/v5/aez5_software.zip,
 * sha256 c06dc3dd251cb777286f503d49069db54b44f6aa652f61203d2c44f6d3929b55, directory
 * crypto_aead/aezv5/ref/). Build and run from that directory:
 *
 *   cc -O1 -w -I<dir of this file> -I. -o generate <this file> encrypt.c blake2b.c rijndael-alg-fst.c
 *   ./generate > reference.txt
 *
 * One vector per line, six space-separated fields; "-" is the empty string:
 *   key nonce ad tau message ciphertext
 * where ad is "<count>" followed by ":<hex>" per component and tau is in bytes.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "crypto_aead.h"

typedef unsigned char byte;
void Encrypt(byte *K, unsigned kbytes, byte *N, unsigned nbytes, byte *AD[], unsigned adbytes[],
             unsigned veclen, unsigned abytes, byte *M, unsigned mbytes, byte *C);
int Decrypt(byte *K, unsigned kbytes, byte *N, unsigned nbytes, byte *AD[], unsigned adbytes[],
            unsigned veclen, unsigned abytes, byte *C, unsigned cbytes, byte *M);

/* xorshift64: a fixed, portable byte stream so the file is reproducible. */
static unsigned long long state = 0x9E3779B97F4A7C15ULL;
static byte next_byte(void) { state ^= state << 13; state ^= state >> 7; state ^= state << 17; return (byte)(state >> 24); }
static void fill(byte *p, unsigned n) { for (unsigned i = 0; i < n; i++) p[i] = next_byte(); }
static void hex(const byte *p, unsigned n) { if (!n) putchar('-'); for (unsigned i = 0; i < n; i++) printf("%02x", p[i]); }

static void emit(byte *K, unsigned kl, byte *N, unsigned nl, byte **AD, unsigned *adl, unsigned v,
                 unsigned tau, byte *M, unsigned ml) {
  byte *C = malloc(ml + tau + 1), *R = malloc(ml + 1);
  Encrypt(K, kl, N, nl, AD, adl, v, tau, M, ml, C);
  if (Decrypt(K, kl, N, nl, AD, adl, v, tau, C, ml + tau, R) != 0 || memcmp(M, R, ml)) {
    fprintf(stderr, "reference round trip failed\n");
    exit(1);
  }
  hex(K, kl); putchar(' '); hex(N, nl); putchar(' ');
  printf("%u", v); for (unsigned i = 0; i < v; i++) { putchar(':'); hex(AD[i], adl[i]); }
  printf(" %u ", tau); hex(M, ml); putchar(' '); hex(C, ml + tau); putchar('\n');
  free(C); free(R);
}

int main(void) {
  byte K[48], N[32], A[3][80], M[512];
  byte *AD[3] = {A[0], A[1], A[2]};
  unsigned adl[3];
  unsigned long long cl;

  /* 1. Length sweep in the #834 shape: raw 48-byte key, empty tweak, tau in {0, 16},
        every message length 0..=160 (AEZ-tiny, the tiny/core boundary, 0..4 core pairs,
        and every |M_uv| residue). */
  unsigned sweep_tau[] = {0, 16};
  for (unsigned t = 0; t < 2; t++)
    for (unsigned ml = 0; ml <= 160; ml++) {
      fill(K, 48); fill(M, ml);
      emit(K, 48, N, 0, AD, adl, 0, sweep_tau[t], M, ml);
    }

  /* 2. Grid: tau x message length, with random nonce (0..32 bytes) and 0..3 AD components
        (lengths drawn from both block multiples and arbitrary residues). */
  unsigned grid_tau[] = {0, 1, 4, 16, 32};
  unsigned grid_len[] = {0, 1, 2, 3, 7, 8, 15, 16, 17, 31, 32, 33, 47, 48, 63, 64, 65,
                         95, 96, 127, 128, 129, 255, 256, 257, 511};
  for (unsigned t = 0; t < 5; t++)
    for (unsigned r = 0; r < 4; r++)
      for (unsigned m = 0; m < sizeof grid_len / sizeof *grid_len; m++) {
        unsigned nl = next_byte() % 33, v = next_byte() % 4;
        fill(K, 48); fill(N, nl);
        for (unsigned i = 0; i < v; i++) {
          adl[i] = (next_byte() % 3 == 0) ? 16 * (next_byte() % 4) : next_byte() % 70;
          fill(A[i], adl[i]);
        }
        fill(M, grid_len[m]);
        emit(K, 48, N, nl, AD, adl, v, grid_tau[t], M, grid_len[m]);
      }

  /* 3. CAESAR genkat through crypto_aead_encrypt (12-byte nonce, one AD component,
        tau = 16): key = 00..2f, nonce = 00..0b, PT and AD = 00.. of every length 0..=32. */
  byte k[48], n[12], p[32], a[32], c[48];
  for (int i = 0; i < 48; i++) k[i] = (byte)i;
  for (int i = 0; i < 12; i++) n[i] = (byte)i;
  for (int i = 0; i < 32; i++) { p[i] = (byte)i; a[i] = (byte)i; }
  for (unsigned pl = 0; pl <= 32; pl++)
    for (unsigned al = 0; al <= 32; al++) {
      crypto_aead_encrypt(c, &cl, p, pl, a, al, 0, n, k);
      hex(k, 48); putchar(' '); hex(n, 12); printf(" 1:"); hex(a, al);
      printf(" 16 "); hex(p, pl); putchar(' '); hex(c, (unsigned)cl); putchar('\n');
    }
  return 0;
}
