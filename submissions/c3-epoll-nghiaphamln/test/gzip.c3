module server;
extern fn int open(char* path, int flags, ...);
fn int main()
{
 char* src = malloc(65536);
 char* dst = malloc(262144);
 int fd = open(getenv("INPUT"), 0);
 sz n = read(fd, src, 65536);
 close(fd);
 if (n < 1024 || n > 65535) return 1;
 gz_init();
 usz zn = gzip(src, (usz)n, dst, 262144);
 if (zn == 0) return 2;
 fd = open(getenv("OUTPUT"), 577, 420);
 if (fd < 0 || write(fd, dst, zn) != (sz)zn) return 3;
 close(fd);
 return 0;
}
