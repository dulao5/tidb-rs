// libmysqlclient smoke/perf client for tidb-rs (build: tools/protocol-smoke/build.sh): N threads, each prepares "SELECT ?" and executes it
// with random ints (checking the echo), or runs COM_QUERY "SELECT 1" in text mode.
#include <mysql.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static int port, secs, text_mode, use_tls;
static long long total;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
#define DIE(m, ...) do { fprintf(stderr, m "\n", __VA_ARGS__); exit(1); } while (0)

static void *worker(void *arg) {
  (void)arg;
  mysql_thread_init();
  MYSQL *c = mysql_init(NULL);
  unsigned int ssl = use_tls ? SSL_MODE_REQUIRED : SSL_MODE_DISABLED;
  mysql_options(c, MYSQL_OPT_SSL_MODE, &ssl);
  if (!mysql_real_connect(c, "127.0.0.1", "root", "", NULL, port, NULL, 0)) DIE("connect: %s", mysql_error(c));
  long long n = 0; double t0 = now(); unsigned seed = (unsigned)(size_t)&n;
  if (text_mode) {
    while (now() - t0 < secs) for (int i = 0; i < 100; i++) {
      if (mysql_query(c, "SELECT 1")) DIE("query: %s", mysql_error(c));
      MYSQL_RES *r = mysql_store_result(c); MYSQL_ROW row = mysql_fetch_row(r);
      if (!row || strcmp(row[0], "1")) DIE("bad row%s", ""); mysql_free_result(r); n++;
    }
  } else {
    MYSQL_STMT *s = mysql_stmt_init(c);
    if (mysql_stmt_prepare(s, "SELECT ?", 8)) DIE("prepare: %s", mysql_stmt_error(s));
    long long in = 0, out = 0; bool is_null = 0;
    MYSQL_BIND p; memset(&p, 0, sizeof p); p.buffer_type = MYSQL_TYPE_LONGLONG; p.buffer = &in;
    MYSQL_BIND r; memset(&r, 0, sizeof r); r.buffer_type = MYSQL_TYPE_LONGLONG; r.buffer = &out; r.is_null = &is_null;
    if (mysql_stmt_bind_param(s, &p)) DIE("bind: %s", mysql_stmt_error(s));
    while (now() - t0 < secs) for (int i = 0; i < 100; i++) {
      in = rand_r(&seed) % 1000000;
      if (mysql_stmt_execute(s)) DIE("execute: %s", mysql_stmt_error(s));
      if (mysql_stmt_bind_result(s, &r)) DIE("bind result: %s", mysql_stmt_error(s));
      if (mysql_stmt_fetch(s)) DIE("fetch: %s", mysql_stmt_error(s));
      if (out != in) DIE("echo mismatch %lld != %lld", out, in);
      if (mysql_stmt_fetch(s) != MYSQL_NO_DATA) DIE("extra row%s", "");
      n++;
    }
    mysql_stmt_close(s);
  }
  mysql_close(c);
  mysql_thread_end();
  pthread_mutex_lock(&mu); total += n; pthread_mutex_unlock(&mu);
  return NULL;
}

int main(int argc, char **argv) {
  if (argc < 5) { fprintf(stderr, "usage: %s port threads secs ps|text [tls]\n", argv[0]); return 2; }
  use_tls = argc > 5 && !strcmp(argv[5], "tls");
  port = atoi(argv[1]); int threads = atoi(argv[2]); secs = atoi(argv[3]); text_mode = !strcmp(argv[4], "text");
  printf("client library %s\n", mysql_get_client_info());
  mysql_library_init(0, NULL, NULL);
  pthread_t th[256]; double t0 = now();
  for (int i = 0; i < threads; i++) pthread_create(&th[i], NULL, worker, NULL);
  for (int i = 0; i < threads; i++) pthread_join(th[i], NULL);
  double el = now() - t0;
  printf("%s%s: %d threads, %lld statements in %.1fs = %.0f qps\n", argv[4], use_tls ? "+tls" : "", threads, total, el, total / el);
  return 0;
}
