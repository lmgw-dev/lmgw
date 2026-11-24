/* Usage: cc -shared -fPIC -o target/convert-perf/no-mallopt.so scripts/convert-perf/no-mallopt.c, then
 * LD_PRELOAD=$PWD/target/convert-perf/no-mallopt.so on the gateway: its mallopt calls do nothing, so
 * the same binary runs with glibc's own (dynamic) thresholds, the "without alloc_tune.rs" rows of
 * design §14 (performance round review S6). The tools lmgw starts get a cleared environment and so
 * never see it. */
int mallopt(int param, int value) { (void)param; (void)value; return 1; }
