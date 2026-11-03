// gzip, tuned for one JSON body per call: greedy LZ77 with a single-probe hash table (one lookup per
// position, no chains, like igzip level 0; matches compared eight bytes at a time), then one dynamic
// Huffman block with trees built for this body (RFC 1951 3.2.7), and a slicing-by-8 CRC.
// Two passes: LZ77 into a token buffer while counting symbols, then the trees and the bits.
module server;

tlocal char* zbuf;
tlocal ushort* gz_head;  // hash -> last position + 1 (0: none); input is capped at 64 KB so positions fit
tlocal uint* gz_tok;     // LZ77 tokens: a literal byte, or MATCH | len << 16 | dist

const usz GZ_MAX_IN = 65535;
const uint GZ_HBITS = 13;
const uint MATCH = 1u << 31;
const uint NLIT = 286;   // literal/length symbols in use (0..285)
const uint NDIST = 30;

uint[8 * 256] crc_tab;   // slicing-by-8: table t at crc_tab[t * 256 ..], built by gz_init()
char[259] len_sym;       // match length -> length symbol index 0..28
char[257] dist_sym_lo;   // distance -> distance symbol: d <= 256 by d ...
char[257] dist_sym_hi;   // ... else by (d - 1) >> 7
uint[29] len_base = { 3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99,
	115, 131, 163, 195, 227, 258 };
char[29] len_extra = { 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0 };
uint[30] dist_base = { 1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025,
	1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577 };
char[30] dist_extra = { 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12,
	12, 13, 13 };
char[19] cl_order = { 16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15 };

fn void gz_init()
{
	for (uint c = 0; c < 29; c++)
	{
		for (uint l = len_base[c]; l < 259 && (c == 28 || l < len_base[c + 1]); l++) len_sym[l] = (char)c;
	}
	for (uint c = 0; c < 30; c++)
	{
		uint lo = dist_base[c];
		uint hi = c == 29 ? 32768 : dist_base[c + 1] - 1;
		for (uint d = lo; d <= hi; d++)
		{
			if (d <= 256)
			{
				dist_sym_lo[d] = (char)c;
			}
			else
			{
				dist_sym_hi[(d - 1) >> 7] = (char)c;
			}
		}
	}
	for (uint i = 0; i < 256; i++)
	{
		uint c = i;
		for (int k = 0; k < 8; k++) c = (c & 1) != 0 ? 0xedb88320 ^ (c >> 1) : c >> 1;
		crc_tab[i] = c;
	}
	for (uint t = 1; t < 8; t++)  // needs the whole of table 0
	{
		for (uint i = 0; i < 256; i++)
		{
			uint prev = crc_tab[(t - 1) * 256 + i];
			crc_tab[t * 256 + i] = crc_tab[prev & 0xff] ^ (prev >> 8);
		}
	}
}

fn uint crc32(char* p, usz n)
{
	uint* t = &crc_tab;
	uint c = 0xffffffff;
	for (; n >= 8; p += 8, n -= 8)
	{
		ulong w;
		memcpy(&w, p, 8);
		uint a = (uint)w ^ c;
		uint b = (uint)(w >> 32);
		c = t[7 * 256 + (a & 0xff)] ^ t[6 * 256 + ((a >> 8) & 0xff)] ^ t[5 * 256 + ((a >> 16) & 0xff)]
		  ^ t[4 * 256 + (a >> 24)] ^ t[3 * 256 + (b & 0xff)] ^ t[2 * 256 + ((b >> 8) & 0xff)]
		  ^ t[1 * 256 + ((b >> 16) & 0xff)] ^ t[b >> 24];
	}
	for (; n != 0; p++, n--) c = t[(c ^ *p) & 0xff] ^ (c >> 8);
	return ~c;
}

// ---------- Huffman code lengths, limited to maxbits, always a complete code (zlib's method) ----------
// freq[s] = 0 leaves symbol s without a code. lens[s] receives the length. Returns the largest length.
fn uint huff_lengths(uint* freq, uint n, uint maxbits, char* lens)
{
	// nodes: leaves first (symbols with freq > 0), then internal nodes; a two-queue merge over sorted leaves
	uint[2 * 286] nf;
	ushort[2 * 286] parent;
	ushort[286] order;
	uint nleaf = 0;
	for (uint s = 0; s < n; s++)
	{
		lens[s] = 0;
		if (freq[s] != 0) order[nleaf++] = (ushort)s;
	}
	if (nleaf == 0) return 0;
	if (nleaf == 1)  // deflate needs at least one 1-bit code; fine for a decoder
	{
		lens[order[0]] = 1;
		return 1;
	}
	for (uint i = 1; i < nleaf; i++)  // insertion sort by frequency: n is small
	{
		ushort s = order[i];
		uint j = i;
		while (j > 0 && freq[order[j - 1]] > freq[s])
		{
			order[j] = order[j - 1];
			j--;
		}
		order[j] = s;
	}
	for (uint i = 0; i < nleaf; i++) nf[i] = freq[order[i]];
	uint li = 0;        // next leaf
	uint ii = nleaf;    // next internal node
	uint next = nleaf;  // where the next internal node goes
	while ((nleaf - li) + (next - ii) > 1)
	{
		uint a;
		uint b;
		if (ii < next && (li >= nleaf || nf[ii] <= nf[li])) { a = ii++; } else { a = li++; }
		if (ii < next && (li >= nleaf || nf[ii] <= nf[li])) { b = ii++; } else { b = li++; }
		nf[next] = nf[a] + nf[b];
		parent[a] = (ushort)next;
		parent[b] = (ushort)next;
		next++;
	}
	uint[32] bl_count = {};
	uint maxlen = 0;
	char[2 * 286] depth;
	depth[next - 1] = 0;
	for (uint i = next - 1; i-- > 0;)  // children come before their parent
	{
		uint d = (uint)depth[parent[i]] + 1;
		depth[i] = (char)d;
		if (i < nleaf)
		{
			if (d > maxlen) maxlen = d;
			bl_count[d < 31 ? d : 31]++;
		}
	}
	if (maxlen > maxbits)
	{
		// too deep: zlib's overflow fix on the length histogram, then reassign lengths in frequency order
		uint overflow = 0;
		for (uint d = maxbits + 1; d <= maxlen && d < 32; d++)
		{
			overflow += bl_count[d];
			bl_count[d] = 0;
		}
		bl_count[maxbits] += overflow;
		do
		{
			uint bits = maxbits - 1;
			while (bl_count[bits] == 0) bits--;
			bl_count[bits]--;
			bl_count[bits + 1] += 2;
			bl_count[maxbits]--;
			overflow -= 2;
		} while ((int)overflow > 0);
		maxlen = maxbits;
		uint k = 0;  // least frequent leaves get the longest codes
		for (uint d = maxbits; d >= 1; d--)
		{
			for (uint c = bl_count[d]; c != 0; c--) depth[k++] = (char)d;
		}
	}
	for (uint i = 0; i < nleaf; i++) lens[order[i]] = depth[i];
	return maxlen;
}

// RFC 1951 3.2.2: codes from lengths, bit-reversed for the LSB-first stream.
fn void canonical(char* lens, uint n, ushort* codes)
{
	uint[16] count = {};
	uint[16] next;
	for (uint s = 0; s < n; s++) count[lens[s]]++;
	count[0] = 0;
	uint c = 0;
	for (uint b = 1; b < 16; b++)
	{
		c = (c + count[b - 1]) << 1;
		next[b] = c;
	}
	for (uint s = 0; s < n; s++)
	{
		if (lens[s] == 0) continue;
		uint v = next[lens[s]]++;
		uint r = 0;
		for (uint i = 0; i < lens[s]; i++)
		{
			r = r << 1 | (v & 1);
			v >>= 1;
		}
		codes[s] = (ushort)r;
	}
}

struct Bits
{
	char* out;
	usz n;
	ulong acc;
	uint cnt;
}

// At most 32 bits per call.
fn void Bits.put(&self, uint code, uint len) @inline
{
	self.acc |= (ulong)code << self.cnt;
	self.cnt += len;
	if (self.cnt >= 32)
	{
		uint w = (uint)self.acc;
		memcpy(self.out + self.n, &w, 4);
		self.n += 4;
		self.acc >>= 32;
		self.cnt -= 32;
	}
}

fn void Bits.flush(&self)
{
	while (self.cnt > 0)
	{
		self.out[self.n++] = (char)self.acc;
		self.acc >>= 8;
		self.cnt = self.cnt > 8 ? self.cnt - 8 : 0;
	}
}

// The dynamic block header: HLIT, HDIST, HCLEN, the code-length code, then both length lists run-length
// coded.
fn void put_header(Bits* b, char* ll, char* dl)
{
	char[NLIT + NDIST] all;
	memcpy(&all, ll, NLIT);
	memcpy(&all[NLIT], dl, NDIST);
	char[NLIT + NDIST] rsym;  // run-length symbols 0..18 ...
	char[NLIT + NDIST] rext;  // ... and their extra bits
	uint nr = 0;
	uint[19] cf = {};
	for (uint i = 0; i < NLIT + NDIST;)
	{
		char v = all[i];
		uint j = i + 1;
		while (j < NLIT + NDIST && all[j] == v) j++;
		uint run = j - i;
		if (v == 0)
		{
			while (run >= 11)
			{
				uint r = run < 138 ? run : 138;
				rsym[nr] = 18;
				rext[nr++] = (char)(r - 11);
				run -= r;
			}
			while (run >= 3)
			{
				uint r = run < 10 ? run : 10;
				rsym[nr] = 17;
				rext[nr++] = (char)(r - 3);
				run -= r;
			}
			while (run > 0)
			{
				rsym[nr] = 0;
				rext[nr++] = 0;
				run--;
			}
		}
		else
		{
			rsym[nr] = v;
			rext[nr++] = 0;
			run--;
			while (run >= 3)
			{
				uint r = run < 6 ? run : 6;
				rsym[nr] = 16;
				rext[nr++] = (char)(r - 3);
				run -= r;
			}
			while (run > 0)
			{
				rsym[nr] = v;
				rext[nr++] = 0;
				run--;
			}
		}
		i = j;
	}
	for (uint k = 0; k < nr; k++) cf[rsym[k]]++;
	char[19] cl;
	ushort[19] cc;
	huff_lengths(&cf, 19, 7, &cl);
	canonical(&cl, 19, &cc);
	uint hclen = 19;
	while (hclen > 4 && cl[cl_order[hclen - 1]] == 0) hclen--;
	b.put(1, 1);  // BFINAL
	b.put(2, 2);  // BTYPE=10
	b.put(NLIT - 257, 5);
	b.put(NDIST - 1, 5);
	b.put(hclen - 4, 4);
	for (uint k = 0; k < hclen; k++) b.put(cl[cl_order[k]], 3);
	for (uint k = 0; k < nr; k++)
	{
		char s = rsym[k];
		b.put(cc[s], cl[s]);
		if (s == 16)
		{
			b.put(rext[k], 2);
		}
		else if (s == 17)
		{
			b.put(rext[k], 3);
		}
		else if (s == 18)
		{
			b.put(rext[k], 7);
		}
	}
}

fn uint gz_hash(char* p) @inline
{
	return ((((uint)p[0] << 16) | ((uint)p[1] << 8) | (uint)p[2]) * 2654435761u) >> (32 - GZ_HBITS);
}

// Length of the common prefix of a and b, at most max (>= 3 is known), eight bytes at a time.
fn usz match_len(char* a, char* b, usz max) @inline
{
	usz len = 3;
	while (len + 8 <= max)
	{
		ulong x;
		ulong y;
		memcpy(&x, a + len, 8);
		memcpy(&y, b + len, 8);
		if (x != y) return len + (usz)((x ^ y).ctz() >> 3);  // little-endian: the first differing byte is the lowest set bit
		len += 8;
	}
	while (len < max && a[len] == b[len]) len++;
	return len;
}

fn uint dist_symbol(uint dist) @inline
{
	return dist <= 256 ? dist_sym_lo[dist] : dist_sym_hi[(dist - 1) >> 7];
}

// Compresses in[0..n) into out (cap bytes). Returns the gzip size, or 0 if it doesn't fit or n is too
// large.
fn usz gzip(char* in, usz n, char* out, usz cap)
{
	// the worst case (all literals, 9 bits each) is under n * 9/8 + a header
	if (n > GZ_MAX_IN || cap < n + n / 8 + 400) return 0;
	if (gz_head == null)
	{
		gz_head = calloc(1u << GZ_HBITS, 2);
		gz_tok = calloc(GZ_MAX_IN + 1, 4);
	}
	memset(gz_head, 0, (1u << GZ_HBITS) * 2);

	// pass 1: LZ77 into tokens, counting symbols
	uint[NLIT] lf = {};
	uint[NDIST] df = {};
	uint nt = 0;
	usz i = 0;
	usz last = n >= 3 ? n - 3 : 0;  // last position with three bytes to hash
	while (i <= last)
	{
		uint h = gz_hash(in + i);
		usz cand = gz_head[h];
		gz_head[h] = (ushort)(i + 1);
		if (cand != 0)
		{
			cand--;
			if (i - cand <= 32768 && in[cand] == in[i] && in[cand + 1] == in[i + 1] && in[cand + 2] == in[i + 2])
			{
				usz len = match_len(in + cand, in + i, n - i < 258 ? n - i : 258);
				uint dist = (uint)(i - cand);
				gz_tok[nt++] = MATCH | (uint)len << 16 | dist;
				lf[257 + len_sym[len]]++;
				df[dist_symbol(dist)]++;
				i += len;
				continue;
			}
		}
		gz_tok[nt++] = in[i];
		lf[in[i]]++;
		i++;
	}
	while (i < n)
	{
		gz_tok[nt++] = in[i];
		lf[in[i]]++;
		i++;
	}
	lf[256]++;

	// trees for this body
	char[NLIT] ll;
	char[NDIST] dl;
	ushort[NLIT] lc;
	ushort[NDIST] dc;
	huff_lengths(&lf, NLIT, 15, &ll);
	canonical(&ll, NLIT, &lc);
	if (huff_lengths(&df, NDIST, 15, &dl) == 0) dl[0] = 1;  // no matches at all: one dummy distance code keeps the header valid
	canonical(&dl, NDIST, &dc);

	// pass 2: the bits
	char[10] gzhdr = { 0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3 };
	memcpy(out, &gzhdr, 10);
	Bits b = { out, 10, 0, 0 };
	put_header(&b, &ll, &dl);
	for (uint k = 0; k < nt; k++)
	{
		uint tk = gz_tok[k];
		if ((tk & MATCH) == 0)
		{
			b.put(lc[tk], ll[tk]);
			continue;
		}
		uint len = (tk >> 16) & 0x1ff;
		uint dist = tk & 0xffff;
		uint ls = len_sym[len];
		b.put(lc[257 + ls], ll[257 + ls]);
		if (len_extra[ls] != 0) b.put(len - len_base[ls], len_extra[ls]);
		uint ds = dist_symbol(dist);
		b.put(dc[ds], dl[ds]);
		if (dist_extra[ds] != 0) b.put(dist - dist_base[ds], dist_extra[ds]);
	}
	b.put(lc[256], ll[256]);
	b.flush();
	uint crc = crc32(in, n);
	uint isz = (uint)n;
	memcpy(out + b.n, &crc, 4);  // little-endian on every target we build for
	memcpy(out + b.n + 4, &isz, 4);
	return b.n + 8;
}
