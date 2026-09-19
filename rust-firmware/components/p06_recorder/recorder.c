/* P0-6 首次异常记录器 —— V1 离线可审查产物（未集成、未刷机）
 *
 * 覆盖边界（必须与实现一起审阅）：
 *   只能捕获"成功到达异常表分发"的异常。若异常在向量入口 / 建帧 / _xt_context_save /
 *   PS 改写（EXCM 清除）这些步骤中再次出错，则走双重异常路径，本记录器**根本不会被执行**。
 *
 * 原始现场语义：
 *   f_* 全部从传入的 XtExcFrame 复制（该帧由分发器在调用 handler 之前用 EPC_1/PS/EXCSAVE_1
 *   等**原始值**填好）。live_* 是本记录器执行时刻读到的特殊寄存器，**不是**首次异常状态
 *   （PS 已被分发器改写、EXCM 已清），两者分开存放、分开命名。
 *
 * 不保证：IRAM、无日志、无锁**不等于**记录器绝不会再出错；因此用 CLAIMED/DONE 两段状态，
 *   一旦出现 CLAIMED 而未见 DONE，即说明**记录器自身**在写入过程中出错（可检测）。
 */
#define P06_IRAM __attribute__((section(".iram1")))
#define P06_NOINIT __attribute__((section(".rtc_noinit")))

#include "esp_ipc.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

/* 与本轮 ELF 核对的 XtExcFrame 布局一致（窗口 ABI；XCHAL_HAVE_LOOPS 开；SWPRI/OVLY 关）*/
/* P06_LAYOUT_BEGIN （纯注释标记：供主机侧解码器提取布局，不改变代码生成） */
typedef struct {
    unsigned int exit;      /* +0   */
    unsigned int pc;        /* +4   */
    unsigned int ps;        /* +8   */
    unsigned int a0;        /* +12  */
    unsigned int a1;        /* +16  */
    unsigned int a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14, a15; /* +20..+72 */
    unsigned int sar;       /* +76  */
    unsigned int exccause;  /* +80  */
    unsigned int excvaddr;  /* +84  */
    unsigned int lbeg;      /* +88  */
    unsigned int lend;      /* +92  */
    unsigned int lcount;    /* +96  */
    unsigned int tmp0, tmp1, tmp2; /* +100..+108 => 共 112 = 0x70 */
} p06_frame_t;

#define P06_CHECK_OFF(f, o) _Static_assert(__builtin_offsetof(p06_frame_t, f) == (o), #f " offset")
P06_CHECK_OFF(pc, 4);  P06_CHECK_OFF(ps, 8);   P06_CHECK_OFF(a0, 12);
P06_CHECK_OFF(a1, 16); P06_CHECK_OFF(a8, 44);  P06_CHECK_OFF(sar, 76);
P06_CHECK_OFF(exccause, 80); P06_CHECK_OFF(excvaddr, 84);
P06_CHECK_OFF(lcount, 96);   P06_CHECK_OFF(tmp2, 108);
_Static_assert(sizeof(p06_frame_t) == 112, "XtExcFrame size");

#define P06_MAGIC   0x50303631u   /* "P061" */
#define P06_ST_EMPTY   0x00000000u
#define P06_ST_CLAIMED 0x434C4149u /* "CLAI" */
#define P06_ST_DONE    0x444F4E45u /* "DONE" */
#define P06_CAUSES  32
#define P06_SLOTS   8             /* 0..1 每核首录；其余滚动 */

typedef struct {
    unsigned int state;      /* EMPTY -> CLAIMED -> DONE */
    unsigned int core;       /* rsr.prid */
    unsigned int cause;
    unsigned int seq;
    /* —— 原始现场：从 XtExcFrame 复制 —— */
    unsigned int f_pc, f_ps, f_a0, f_a1, f_a2, f_a3, f_a4, f_a5;
    unsigned int f_a6, f_a7, f_a8, f_a9, f_a10, f_a11, f_a12, f_a13, f_a14, f_a15;
    unsigned int f_sar, f_exccause, f_excvaddr, f_lbeg, f_lend, f_lcount, f_exit;
    unsigned int frame_ptr;
    unsigned int prev_handler;
    /* —— 记录时刻的特殊寄存器（**不是**首次异常状态，独立标注）—— */
    unsigned int live_ps, live_epc1, live_exccause, live_excvaddr;
    unsigned int panic_abort_details;
} p06_rec_t;

typedef struct {
    unsigned int magic;
    unsigned int nonce;      /* 每次启动一个新值；用于判定旧内容 */
    unsigned int boot_done;
    unsigned int written;    /* 已记录条目数（非原子，仅诊断） */
    p06_rec_t prev[P06_SLOTS];   /* 上一启动周期内容（供回读） */
    p06_rec_t slots[P06_SLOTS];
} p06_region_t;
/* P06_LAYOUT_END */

P06_NOINIT p06_region_t g_p06;
unsigned int g_p06_nonce;
unsigned int g_p06_state[P06_SLOTS];
unsigned int g_p06_seq[2];
unsigned int g_p06_missed[2];            /* 本启动周期的 nonce（DRAM 副本） */
void *g_p06_prev[2][P06_CAUSES];     /* 每核的实际旧 handler */
unsigned int g_p06_install_mask;
unsigned int g_p06_verify_mask;
unsigned int g_p06_verify_fail[2];
extern const char *g_panic_abort_details;

static __attribute__((always_inline)) inline P06_IRAM int
p06_cas(volatile unsigned int *p, unsigned int expect, unsigned int want)
{
    /* 语义（以 libatomic __atomic_s32c1i_compare_exchange_4 为准）：
     *   wsr.scompare1 <- expect ; s32c1i at <- want ; 结果 at = 旧值
     *   成功判据是 (旧值 == expect)，不是 (旧值 == want)。 */
    unsigned int old = want;              /* "+a" 使其成为真正的输入 */
    __asm__ __volatile__("wsr.scompare1 %1\n\ts32c1i %0, %2, 0"
                         : "+a"(old) : "a"(expect), "a"(p) : "memory");
    return old == expect;
}

static __attribute__((always_inline)) inline P06_IRAM unsigned int p06_core(void)
{
    unsigned int c;
    __asm__ __volatile__("rsr.prid %0" : "=a"(c));
    return (c >> 13) & 1u;   /* S3: PRID bit 13（见 xt_utils.h） */
}

static __attribute__((always_inline)) inline P06_IRAM unsigned int p06_live_ps(void)
{ unsigned int v; __asm__ __volatile__("rsr.ps %0" : "=a"(v)); return v; }
static __attribute__((always_inline)) inline P06_IRAM unsigned int p06_live_epc1(void)
{ unsigned int v; __asm__ __volatile__("rsr.epc1 %0" : "=a"(v)); return v; }
static __attribute__((always_inline)) inline P06_IRAM unsigned int p06_live_exccause(void)
{ unsigned int v; __asm__ __volatile__("rsr.exccause %0" : "=a"(v)); return v; }
static __attribute__((always_inline)) inline P06_IRAM unsigned int p06_live_excvaddr(void)
{ unsigned int v; __asm__ __volatile__("rsr.excvaddr %0" : "=a"(v)); return v; }

/* 单槽写入：CAS 抢占 -> 写字段 -> memw -> DONE */
static __attribute__((always_inline)) inline P06_IRAM void p06_fill(p06_rec_t *r, const p06_frame_t *f, unsigned int cause,
                             unsigned int core, unsigned int seq, void *prev)
{
    r->core = core; r->cause = cause; r->seq = seq;
    r->f_pc = f->pc; r->f_ps = f->ps; r->f_a0 = f->a0; r->f_a1 = f->a1;
    r->f_a2 = f->a2; r->f_a3 = f->a3; r->f_a4 = f->a4; r->f_a5 = f->a5;
    r->f_a6 = f->a6; r->f_a7 = f->a7; r->f_a8 = f->a8; r->f_a9 = f->a9;
    r->f_a10 = f->a10; r->f_a11 = f->a11; r->f_a12 = f->a12; r->f_a13 = f->a13;
    r->f_a14 = f->a14; r->f_a15 = f->a15;
    r->f_sar = f->sar; r->f_exccause = f->exccause; r->f_excvaddr = f->excvaddr;
    r->f_lbeg = f->lbeg; r->f_lend = f->lend; r->f_lcount = f->lcount;
    r->f_exit = f->exit; r->frame_ptr = (unsigned int)(unsigned long)f;
    r->prev_handler = (unsigned int)(unsigned long)prev;
    r->live_ps = p06_live_ps(); r->live_epc1 = p06_live_epc1();
    r->live_exccause = p06_live_exccause(); r->live_excvaddr = p06_live_excvaddr();
    r->panic_abort_details = (unsigned int)(unsigned long)g_panic_abort_details;
}

/* 记录路径：CAS + 存储，无 call、无 flash 访问、不写任何特殊寄存器 */
extern void xt_unhandled_exception(p06_frame_t *frame);   /* 表内默认项 */

#define P06_BODY(N)                                                              \
    unsigned int core = p06_core();                                              \
    unsigned int ci = (core < 2u) ? core : 1u;                                   \
    void *prev = g_p06_prev[ci][N];                                              \
    if (g_p06.magic != P06_MAGIC || g_p06.nonce != g_p06_nonce) {                \
        g_p06_missed[ci]++;                                                      \
    } else {                                                                     \
        if (p06_cas(&g_p06_state[ci], P06_ST_EMPTY, P06_ST_CLAIMED)) {            \
            p06_rec_t *r = &g_p06.slots[ci];                                     \
            p06_fill(r, frame, (N), core, ++g_p06_seq[ci], prev);                \
            __sync_synchronize();                                                \
            r->state = P06_ST_DONE;                                              \
        }                                                                         \
    }                                                                             \
    if (prev) { ((void (*)(p06_frame_t *))prev)(frame); }                       \
    else      { xt_unhandled_exception(frame); }  /* prev==0 => 默认处理 */     \
    return;

#define P06_HANDLER(N) P06_IRAM void p06_h##N(p06_frame_t *frame) { P06_BODY(N) }
P06_HANDLER(0)  P06_HANDLER(1)  P06_HANDLER(2)  P06_HANDLER(3)
P06_HANDLER(4)  P06_HANDLER(5)  P06_HANDLER(6)  P06_HANDLER(7)
P06_HANDLER(8)  P06_HANDLER(9)  P06_HANDLER(10) P06_HANDLER(11)
P06_HANDLER(12) P06_HANDLER(13) P06_HANDLER(14) P06_HANDLER(15)
P06_HANDLER(16) P06_HANDLER(17) P06_HANDLER(18) P06_HANDLER(19)
P06_HANDLER(20) P06_HANDLER(21) P06_HANDLER(22) P06_HANDLER(23)
P06_HANDLER(24) P06_HANDLER(25) P06_HANDLER(26) P06_HANDLER(27)
P06_HANDLER(28) P06_HANDLER(29) P06_HANDLER(30) P06_HANDLER(31)

/* 启动期初始化（普通上下文，非异常路径）：保留上一周期内容 -> 开新周期 */
P06_IRAM void p06_boot_init(unsigned int nonce)
{
    unsigned int *dst = (unsigned int *)&g_p06.prev[0];
    unsigned int *src = (unsigned int *)&g_p06.slots[0];
    unsigned int words = (unsigned int)(sizeof(p06_rec_t) / sizeof(unsigned int)) * P06_SLOTS;
    if (g_p06.magic == P06_MAGIC) {
        for (unsigned int i = 0; i < words; ++i) dst[i] = src[i];   /* 显式逐字，避免 memcpy */
    } else {
        for (unsigned int i = 0; i < P06_SLOTS; ++i) g_p06.prev[i].state = P06_ST_EMPTY;
    }
    for (unsigned int i = 0; i < P06_SLOTS; ++i) { g_p06.slots[i].state = P06_ST_EMPTY; g_p06_state[i] = P06_ST_EMPTY; }
    g_p06_seq[0] = 0; g_p06_seq[1] = 0;
    g_p06.nonce = nonce;
    g_p06.boot_done = 1;
    g_p06_nonce = nonce;
    __sync_synchronize();
    g_p06.magic = P06_MAGIC;
    __sync_synchronize();
}


/* ===================== 组件集成部分（相对 V1 产物新增） ===================== */
typedef void (*p06_exc_handler_t)(p06_frame_t *);

static p06_exc_handler_t const p06_handlers[P06_CAUSES] = {
    p06_h0,  p06_h1,  p06_h2,  p06_h3,  p06_h4,  p06_h5,  p06_h6,  p06_h7,
    p06_h8,  p06_h9,  p06_h10, p06_h11, p06_h12, p06_h13, p06_h14, p06_h15,
    p06_h16, p06_h17, p06_h18, p06_h19, p06_h20, p06_h21, p06_h22, p06_h23,
    p06_h24, p06_h25, p06_h26, p06_h27, p06_h28, p06_h29, p06_h30, p06_h31,
};

/* nonce：自由运行周期计数器，用于判定 RTC NOINIT 中的旧内容 */
static P06_IRAM unsigned int p06_make_nonce(void)
{
    unsigned int v;
    __asm__ __volatile__("rsr.ccount %0" : "=a"(v));
    return v | 1u;
}

static void p06_install_on_current_core(void *arg)
{
    (void)arg;
    unsigned int core = p06_core();
    unsigned int ci = (core < 2u) ? core : 1u;
    for (int c = 0; c < P06_CAUSES; ++c) {
        g_p06_prev[ci][c] =
            (void *)xt_set_exception_handler(c, (xt_exc_handler)p06_handlers[c]);
    }
    __sync_synchronize();
    g_p06_install_mask |= 1u << ci;

    unsigned int failures = 0;
    for (int c = 0; c < P06_CAUSES; ++c) {
        xt_exc_handler current =
            xt_set_exception_handler(c, (xt_exc_handler)p06_handlers[c]);
        if (current != (xt_exc_handler)p06_handlers[c]) failures |= 1u << c;
    }
    g_p06_verify_fail[ci] = failures;
    __sync_synchronize();
    g_p06_verify_mask |= 1u << ci;
}

__attribute__((noinline)) void p06_v2_trigger(void)
{
    /* 从地址 0 读取：内联汇编，避免 C 层解引用的未定义行为。
     * 期望：EXCVADDR = 0；EXCCAUSE 由实测与帧对照决定（28 仅为待验预期）。
     * 核心判据是记录器是否忠实复制现场，而非原因码本身。 */
    __asm__ __volatile__(
        "movi a8, 0\n\t"
        "l32i a9, a8, 0\n\t"
        ::: "a8", "a9", "memory");
}

static void p06_v2_trigger_ipc(void *arg)
{
    (void)arg;
    p06_v2_trigger();
}

void p06_v2_trigger_core1(void)
{
#if CONFIG_FREERTOS_NUMBER_OF_CORES > 1
    (void)esp_ipc_call_blocking(1, p06_v2_trigger_ipc, 0);
#else
    p06_v2_trigger();
#endif
}

__attribute__((noinline)) void p06_v3_trigger2(void)
{
    /* V3 第二次故障：读另一处无效地址（0x10），与第一次(0)不同且可从 ELF 定位。
     * 仅由验证固件调用一次；首条记录已 DONE，期望本帧不覆盖首录。 */
    __asm__ __volatile__(
        "movi a8, 0x10\n\t"
        "l32i a9, a8, 0\n\t"
        ::: "a8", "a9", "memory");
}

/* 显式入口：由验证固件在可审计的位置调用（不使用构造器，避免初始化顺序不确定） */
P06_IRAM unsigned int p06_arm(void)
{
    p06_boot_init(p06_make_nonce());
    g_p06_install_mask = 0;
    g_p06_verify_mask = 0;
    g_p06_verify_fail[0] = 0;
    g_p06_verify_fail[1] = 0;

    unsigned int current = p06_core();
    p06_install_on_current_core(0);
#if CONFIG_FREERTOS_NUMBER_OF_CORES > 1
    unsigned int other = current ^ 1u;
    if (esp_ipc_call_blocking(other, p06_install_on_current_core, 0) != ESP_OK) {
        return 0;
    }
#endif
    __sync_synchronize();
    unsigned int expected = (1u << CONFIG_FREERTOS_NUMBER_OF_CORES) - 1u;
    if (g_p06_install_mask != expected || g_p06_verify_mask != expected ||
        g_p06_verify_fail[0] != 0 || g_p06_verify_fail[1] != 0) {
        return 0;
    }
    return expected;
}
