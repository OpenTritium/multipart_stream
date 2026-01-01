#!/bin/bash
# 高效并行运行所有模糊测试并生成详细报告
# Usage: ./fuzz/fuzz_test.sh [duration_in_seconds]

set -e

# 配置
FUZZ_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$FUZZ_DIR")"
DURATION="${1:-10800}"  # 默认3小时（发布前推荐时长）

# 内存友好的并行配置
NPROC=$(nproc)

# libFuzzer的并行策略:
# -workers=M: 启动M个并行worker进程（真正的并行！）
# -jobs=N: 每N个worker为一组，共享corpus（通常保持1）
#
# 最佳配置: -workers=3 -jobs=1
# = 每个target启动3个独立进程，真正并行fuzzing
NUM_TARGETS=$(cargo fuzz list | wc -l)
WORKERS_PER_TARGET=$((NPROC / NUM_TARGETS))

# 限制最大workers，避免内存爆炸
MAX_WORKERS=6
if [ "$WORKERS_PER_TARGET" -gt "$MAX_WORKERS" ]; then
    WORKERS_PER_TARGET="$MAX_WORKERS"
fi

# 最小值保护
[ "$WORKERS_PER_TARGET" -lt 2 ] && WORKERS_PER_TARGET=2

PARALLEL_JOBS="$NPROC"

# 颜色输出
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

# 获取所有fuzz targets
FUZZ_TARGETS=($(cargo fuzz list))

echo -e "${BOLD}${CYAN}╔════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${CYAN}║     Multipart Stream 模糊测试套件                    ║${NC}"
echo -e "${BOLD}${CYAN}╚════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "${BLUE}配置信息:${NC}"
echo -e "  测试时长: ${BOLD}${DURATION}秒${NC} ($(echo "scale=1; $DURATION/60" | bc)分钟)"
echo -e "  CPU核心数: ${BOLD}${NPROC}${NC}"
echo -e "  并发配置: ${BOLD}${NUM_TARGETS} targets × ${WORKERS_PER_TARGET} workers = $((NUM_TARGETS * WORKERS_PER_TARGET)) 进程${NC}"
echo -e "  Fuzz Targets: ${BOLD}${FUZZ_TARGETS[*]}${NC}"
echo ""

# 创建结果目录
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
REPORT_DIR="$FUZZ_DIR/fuzz_reports/$TIMESTAMP"
mkdir -p "$REPORT_DIR"

echo -e "${BLUE}报告目录:${NC} $REPORT_DIR"
echo ""

# 清理旧的corpus artifacts（可选，节省空间）
echo -e "${YELLOW}清理旧的corpus artifacts...${NC}"
find "$FUZZ_DIR/corpus" -name "*.leak" -delete 2>/dev/null || true
find "$FUZZ_DIR/corpus" -name "*.timeout" -delete 2>/dev/null || true
find "$FUZZ_DIR/corpus" -name "*.fuzz" -delete 2>/dev/null || true

# 构建所有fuzz targets
echo -e "${YELLOW}构建所有fuzz targets...${NC}"
BUILD_START=$(date +%s)
for target in "${FUZZ_TARGETS[@]}"; do
    echo -n "  - 构建 $target... "
    if cargo fuzz build "$target" > /dev/null 2>&1; then
        echo -e "${GREEN}✓${NC}"
    else
        echo -e "${RED}✗${NC}"
        exit 1
    fi
done
BUILD_END=$(date +%s)
BUILD_TIME=$((BUILD_END - BUILD_START))
echo -e "${GREEN}✓ 所有targets构建完成 (耗时: ${BUILD_TIME}秒)${NC}"
echo ""

# 记录每个target的初始corpus大小
declare -A INITIAL_CORPUS
for target in "${FUZZ_TARGETS[@]}"; do
    INITIAL_CORPUS[$target]=$(find "$FUZZ_DIR/corpus/$target" -type f 2>/dev/null | wc -l)
done

# 启动所有并行fuzz测试
echo -e "${YELLOW}启动并行模糊测试...${NC}"
echo ""

PIDS=()
LOG_FILES=()
START_TIME=$(date +%s)
FAILED=()

for target in "${FUZZ_TARGETS[@]}"; do
    LOG_FILE="$REPORT_DIR/${target}.log"
    LOG_FILES+=("$LOG_FILE")

    echo -e "  [${YELLOW}启动${NC}] $target"

    # libFuzzer并行配置:
    # -workers=3: 启动3个独立的并行fuzzing进程（真正并行！）
    # -jobs=1: 所有workers共享同一个corpus
    # -max_len: 限制输入大小，避免大样本占用过多内存
    #
    # 每个target会启动3个独立进程并行fuzzing
    # 总进程数: 4 targets × 3 workers = 12 个活跃进程
    cargo fuzz run "$target" \
        -- -timeout=5 \
        -max_total_time="$DURATION" \
        -jobs=1 \
        -workers="$WORKERS_PER_TARGET" \
        -max_len=1048576 \
        -print_final_stats=1 \
        > "$LOG_FILE" 2>&1 &

    PIDS+=($!)
done

echo ""
echo -e "${GREEN}✓ 所有模糊测试已在后台运行${NC}"
echo -e "${BLUE}💡 提示: 使用 'tail -f $REPORT_DIR/*.log' 查看实时日志${NC}"
echo ""

# 等待所有测试完成
echo -e "${YELLOW}⏳ 等待测试完成...${NC}"
for i in "${!FUZZ_TARGETS[@]}"; do
    target="${FUZZ_TARGETS[$i]}"
    pid="${PIDS[$i]}"
    log="${LOG_FILES[$i]}"

    if wait $pid; then
        echo -e "  ${GREEN}✓${NC} $target 完成"
    else
        echo -e "  ${RED}✗${NC} $target 失败 (退出码: $?)"
        FAILED+=("$target")
    fi
done

END_TIME=$(date +%s)
TOTAL_TIME=$((END_TIME - START_TIME))

echo ""
echo -e "${BOLD}${GREEN}════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}${GREEN}              测试报告生成中...                        ${NC}"
echo -e "${BOLD}${GREEN}════════════════════════════════════════════════════════${NC}"
echo ""

# 生成详细的Markdown报告
MD_REPORT="$REPORT_DIR/REPORT.md"
{
    echo "# Multipart Stream 模糊测试报告"
    echo ""
    echo "## 📊 测试概览"
    echo ""
    echo "| 项目 | 详情 |"
    echo "|------|------|"
    echo "| **测试时间** | $(date '+%Y-%m-%d %H:%M:%S') |"
    echo "| **测试时长** | ${DURATION}秒 ($(echo "scale=1; $DURATION/60" | bc)分钟) |"
    echo "| **实际耗时** | ${TOTAL_TIME}秒 |"
    echo "| **并行任务** | ${PARALLEL_JOBS} |"
    echo "| **Fuzz Targets** | ${#FUZZ_TARGETS[@]} |"
    echo "| **测试状态** | $([ ${#FAILED[@]} -eq 0 ] && echo "✅ 通过" || echo "❌ 失败") |"
    echo ""
    echo "## 🎯 测试结果"
    echo ""
    echo "| Target | 总执行次数 | 执行速度(exec/s) | 覆盖率(cov) | 特征数(ft) | Corpus大小(KB) | 新增样本 | Crashes | 超时 | 泄漏 | 状态 |"
    echo "|--------|-----------|----------------|------------|----------|--------------|---------|---------|------|------|------|"

    for target in "${FUZZ_TARGETS[@]}"; do
        log="$REPORT_DIR/${target}.log"
        corpus_dir="$FUZZ_DIR/corpus/$target"

        # 统计信息
        corpus_final=$(find "$corpus_dir" -type f 2>/dev/null | wc -l)
        corpus_initial=${INITIAL_CORPUS[$target]}
        new_samples=$((corpus_final - corpus_initial))

        # 从日志中提取libFuzzer最终统计信息
        if [ -f "$log" ]; then
            # 提取最终统计行（通常在日志末尾）
            final_stats=$(tail -30 "$log" | grep -E "^#[0-9]+.*cov:" | tail -1)

            if [ -n "$final_stats" ]; then
                # 解析libFuzzer统计格式: #1234 cov: 5678 ft: 1234 corp: 45/123kb lim: 4096 exec/s: 5678 rss: 123Mb
                execs_done=$(echo "$final_stats" | grep -oP '^\#\K[0-9]+' || echo "N/A")
                exec_per_sec=$(echo "$final_stats" | grep -oP "exec/s: \K[0-9]+" || echo "N/A")
                coverage=$(echo "$final_stats" | grep -oP "cov: \K[0-9]+" || echo "N/A")
                features=$(echo "$final_stats" | grep -oP "ft: \K[0-9]+" || echo "N/A")
                corpus_info=$(echo "$final_stats" | grep -oP "corp: [^ ]+" || echo "N/A")
            else
                execs_done="N/A"
                exec_per_sec="N/A"
                coverage="N/A"
                features="N/A"
                corpus_info="N/A"
            fi

            # Corpus文件大小统计
            corpus_size_bytes=$(find "$corpus_dir" -type f -exec du -b {} + 2>/dev/null | awk '{sum+=$1} END {print sum}' || echo "0")
            corpus_size_kb=$((corpus_size_bytes / 1024))

            crashes=$(find "$corpus_dir" -name "crash-*" -type f 2>/dev/null | wc -l)
            timeouts=$(find "$corpus_dir" -name "timeout-*" -type f 2>/dev/null | wc -l)
            leaks=$(find "$corpus_dir" -name "leak-*" -type f 2>/dev/null | wc -l)

            # 判断状态
            if [[ " ${FAILED[@]} " =~ " ${target} " ]]; then
                status="❌ 失败"
            elif [ "$crashes" -gt 0 ]; then
                status="⚠️  发现crash"
            elif [ "$leaks" -gt 0 ]; then
                status="⚠️  内存泄漏"
            elif [ "$timeouts" -gt 0 ]; then
                status="⚠️  有超时"
            else
                status="✅ 通过"
            fi
        else
            execs_done="N/A"
            exec_per_sec="N/A"
            coverage="N/A"
            features="N/A"
            corpus_info="N/A"
            corpus_size_kb="N/A"
            crashes="N/A"
            timeouts="N/A"
            leaks="N/A"
            status="❓ 未知"
        fi

        echo "| \`$target\` | $execs_done | $exec_per_sec | $coverage | $features | $corpus_size_kb | +$new_samples | $crashes | $timeouts | $leaks | $status |"
    done

    echo ""
    echo "## 📈 详细统计"
    echo ""

    for target in "${FUZZ_TARGETS[@]}"; do
        log="$REPORT_DIR/${target}.log"
        corpus_dir="$FUZZ_DIR/corpus/$target"
        echo "### \`${target}\`"
        echo ""

        if [ -f "$log" ]; then
            # 提取最终统计行
            final_stats=$(tail -30 "$log" | grep -E "^#[0-9]+.*cov:" | tail -1)

            if [ -n "$final_stats" ]; then
                echo "**📊 最终统计:**"
                echo "\`\`\`"
                echo "$final_stats"
                echo "\`\`\`"
                echo ""

                # 解析并显示详细指标
                execs_done=$(echo "$final_stats" | grep -oP '^\#\K[0-9]+')
                exec_per_sec=$(echo "$final_stats" | grep -oP "exec/s: \K[0-9]+")
                coverage=$(echo "$final_stats" | grep -oP "cov: \K[0-9]+")
                features=$(echo "$final_stats" | grep -oP "ft: \K[0-9]+")
                corp_files=$(echo "$final_stats" | grep -oP "corp: \K[0-9]+")

                echo "**📈 关键指标:**"
                echo "- **总执行次数:** $(printf "%'d" ${execs_done:-N/A})"
                echo "- **执行速度:** $(printf "%'d" ${exec_per_sec:-N/A}) exec/s"
                echo "- **代码覆盖:** ${coverage:-N/A} edges"
                echo "- **特征数量:** $(printf "%'d" ${features:-N/A})"
                echo "- **Corpus文件:** ${corp_files:-N/A} 个"
                echo ""
            fi

            # Corpus质量分析
            corpus_files=$(find "$corpus_dir" -type f 2>/dev/null)
            if [ -n "$corpus_files" ]; then
                echo "**📦 Corpus质量:**"
                total_size=$(find "$corpus_dir" -type f -exec du -b {} + 2>/dev/null | awk '{sum+=$1} END {print sum}')
                avg_size=$((total_size / $(echo "$corpus_files" | wc -l)))
                max_size=$(find "$corpus_dir" -type f -exec du -b {} + 2>/dev/null | sort -n | tail -1 | cut -f1)
                min_size=$(find "$corpus_dir" -type f -exec du -b {} + 2>/dev/null | sort -n | head -1 | cut -f1)

                echo "- **总大小:** $(printf "%'d" $((total_size / 1024))) KB"
                echo "- **平均大小:** $(printf "%'d" $avg_size) bytes"
                echo "- **最大文件:** $(printf "%'d" $max_size) bytes"
                echo "- **最小文件:** $(printf "%'d" $min_size) bytes"
                echo ""
            fi

            # 提取错误信息（如果有）
            if grep -q "panic\|error\|Error\|ERROR" "$log"; then
                echo "**❌ 错误信息:**"
                echo "\`\`\`"
                grep -A 10 "panic\|error\|Error\|ERROR" "$log" | head -50
                echo "\`\`\`"
                echo ""
            fi

            # 显示crash详情
            crash_count=$(find "$corpus_dir" -name "crash-*" -type f 2>/dev/null | wc -l)
            if [ "$crash_count" -gt 0 ]; then
                echo "**🐛 Crash文件 ($crash_count 个):**"
                find "$corpus_dir" -name "crash-*" -type f 2>/dev/null | while read -r crash; do
                    size=$(stat -c%s "$crash")
                    echo "  - \`$(basename "$crash")\` ($(printf "%'d" $size) bytes)"
                done
                echo ""
            fi
        else
            echo "❌ 日志文件不存在"
            echo ""
        fi
    done

    echo "## 📁 生成的文件"
    echo ""
    echo "| 文件 | 描述 |"
    echo "|------|------|"
    for log in "${LOG_FILES[@]}"; do
        filename=$(basename "$log")
        echo "| [\`$filename\`]($filename) | $filename 详细日志 |"
    done
    echo ""

    if [ ${#FAILED[@]} -gt 0 ]; then
        echo "## ❌ 失败的Targets"
        echo ""
        for target in "${FAILED[@]}"; do
            echo "- \`$target\`"
        done
        echo ""
    fi

    # Crash文件分析和分类
    TOTAL_CRASHES=0
    TOTAL_TIMEOUTS=0
    TOTAL_LEAKS=0
    declare -A CRASH_TYPES

    echo "## 🐛 问题分析"
    echo ""

    for target in "${FUZZ_TARGETS[@]}"; do
        crash_dir="$FUZZ_DIR/corpus/$target"
        log="$REPORT_DIR/${target}.log"

        if [ -d "$crash_dir" ]; then
            crash_files=$(find "$crash_dir" -name "crash-*" -type f 2>/dev/null)
            timeout_files=$(find "$crash_dir" -name "timeout-*" -type f 2>/dev/null)
            leak_files=$(find "$crash_dir" -name "leak-*" -type f 2>/dev/null)

            crash_count=$(echo "$crash_files" | wc -l)
            timeout_count=$(echo "$timeout_files" | wc -l)
            leak_count=$(echo "$leak_files" | wc -l)

            TOTAL_CRASHES=$((TOTAL_CRASHES + crash_count))
            TOTAL_TIMEOUTS=$((TOTAL_TIMEOUTS + timeout_count))
            TOTAL_LEAKS=$((TOTAL_LEAKS + leak_count))

            if [ "$crash_count" -gt 0 ] || [ "$timeout_count" -gt 0 ] || [ "$leak_count" -gt 0 ]; then
                echo "### \`$target\`"
                echo ""

                # 分析崩溃类型
                if [ "$crash_count" -gt 0 ]; then
                    echo "**Crashes ($crash_count):**"

                    # 从日志中提取崩溃原因
                    if [ -f "$log" ]; then
                        # 尝试识别常见崩溃类型
                        if grep -q "panic\|assert" "$log"; then
                            echo "- 类型: **Panic/Assert**"
                            CRASH_TYPES["Panic/Assert"]=$(( ${CRASH_TYPES["Panic/Assert"]:-0} + crash_count ))
                        elif grep -q "out of bounds\|index.*out.*of.*bounds" "$log"; then
                            echo "- 类型: **越界访问**"
                            CRASH_TYPES["越界访问"]=$(( ${CRASH_TYPES["越界访问"]:-0} + crash_count ))
                        elif grep -q "segmentation fault\|segfault" "$log"; then
                            echo "- 类型: **段错误**"
                            CRASH_TYPES["段错误"]=$(( ${CRASH_TYPES["段错误"]:-0} + crash_count ))
                        elif grep -q "null pointer\|NULL pointer" "$log"; then
                            echo "- 类型: **空指针解引用**"
                            CRASH_TYPES["空指针解引用"]=$(( ${CRASH_TYPES["空指针解引用"]:-0} + crash_count ))
                        else
                            echo "- 类型: **其他崩溃**"
                            CRASH_TYPES["其他"]=$(( ${CRASH_TYPES["其他"]:-0} + crash_count ))
                        fi
                    fi

                    echo ""
                    echo "| 文件 | 大小 |"
                    echo "|------|------|"
                    echo "$crash_files" | while read -r crash; do
                        if [ -n "$crash" ]; then
                            size=$(stat -c%s "$crash")
                            echo "| [\`$(basename "$crash")\`](../../corpus/$target/$(basename "$crash")) | $(printf "%'d" $size) bytes |"
                        fi
                    done
                    echo ""
                fi

                if [ "$timeout_count" -gt 0 ]; then
                    echo "**超时 ($timeout_count):**"
                    echo "- 考虑增加 \`-timeout\` 参数值或优化目标函数"
                    echo ""
                fi

                if [ "$leak_count" -gt 0 ]; then
                    echo "**内存泄漏 ($leak_count):**"
                    echo "- 使用 \`-leak=1\` 参数检测到的内存泄漏"
                    echo ""
                fi
            fi
        fi
    done

    if [ $TOTAL_CRASHES -eq 0 ] && [ $TOTAL_TIMEOUTS -eq 0 ] && [ $TOTAL_LEAKS -eq 0 ]; then
        echo "✅ **未发现问题**"
        echo ""
    fi

    echo "## 📊 总体统计"
    echo ""
    echo "| 指标 | 数值 |"
    echo "|------|------|"
    echo "| **总测试时间** | ${DURATION}秒 |"
    echo "| **总corpus大小** | $(find "$FUZZ_DIR/corpus" -type f 2>/dev/null | wc -l) 个文件 |"
    echo "| **Crashes** | $TOTAL_CRASHES |"
    echo "| **超时** | $TOTAL_TIMEOUTS |"
    echo "| **内存泄漏** | $TOTAL_LEAKS |"
    echo "| **通过率** | $(echo "scale=1; (${#FUZZ_TARGETS[@]} - ${#FAILED[@]}) * 100 / ${#FUZZ_TARGETS[@]}" | bc)% |"
    echo ""

    # 如果有崩溃，显示崩溃类型汇总
    if [ $TOTAL_CRASHES -gt 0 ] && [ ${#CRASH_TYPES[@]} -gt 0 ]; then
        echo "### 崩溃类型分布"
        echo ""
        for crash_type in "${!CRASH_TYPES[@]}"; do
            echo "- **$crash_type**: ${CRASH_TYPES[$crash_type]}"
        done
        echo ""
    fi
    echo "---"
    echo ""
    echo "*报告生成时间: $(date)*"
    echo "*生成工具: fuzz_test.sh*"

} > "$MD_REPORT"

# 生成纯文本版本摘要
SUMMARY_FILE="$REPORT_DIR/summary.txt"
{
    echo "═══════════════════════════════════════════════════════════════"
    echo "         Multipart Stream 模糊测试报告"
    echo "═══════════════════════════════════════════════════════════════"
    echo ""
    echo "测试时间: $(date '+%Y-%m-%d %H:%M:%S')"
    echo "测试时长: ${DURATION}秒"
    echo "实际耗时: ${TOTAL_TIME}秒"
    echo "并行任务: ${PARALLEL_JOBS}"
    echo ""
    echo "───────────────────────────────────────────────────────────────"
    echo "测试结果"
    echo "───────────────────────────────────────────────────────────────"
    printf "%-20s %-12s %-12s %-12s %-10s\n" "Target" "总执行次数" "执行速度" "覆盖率" "Crashes"
    printf "%-20s %-12s %-12s %-12s %-10s\n" "--------" "------------" "------------" "------------" "--------"

    total_execs=0
    total_crashes=0

    for target in "${FUZZ_TARGETS[@]}"; do
        log="$REPORT_DIR/${target}.log"
        corpus_dir="$FUZZ_DIR/corpus/$target"

        # 提取最终统计
        final_stats=$(tail -30 "$log" | grep -E "^#[0-9]+.*cov:" | tail -1)

        if [ -n "$final_stats" ]; then
            execs_done=$(echo "$final_stats" | grep -oP '^\#\K[0-9]+' || echo "0")
            exec_per_sec=$(echo "$final_stats" | grep -oP "exec/s: \K[0-9]+" || echo "0")
            coverage=$(echo "$final_stats" | grep -oP "cov: \K[0-9]+" || echo "N/A")
        else
            execs_done="0"
            exec_per_sec="0"
            coverage="N/A"
        fi

        corpus_final=$(find "$corpus_dir" -type f 2>/dev/null | wc -l)
        corpus_initial=${INITIAL_CORPUS[$target]}
        new_samples=$((corpus_final - corpus_initial))
        crashes=$(find "$corpus_dir" -name "crash-*" -type f 2>/dev/null | wc -l)
        timeouts=$(find "$corpus_dir" -name "timeout-*" -type f 2>/dev/null | wc -l)

        total_execs=$((total_execs + execs_done))
        total_crashes=$((total_crashes + crashes))

        # 格式化数字（添加千位分隔符）
        execs_formatted=$(printf "%'d" $execs_done 2>/dev/null || echo "$execs_done")
        exec_sec_formatted=$(printf "%'d" $exec_per_sec 2>/dev/null || echo "$exec_per_sec")

        printf "%-20s %-12s %-12s %-12s" "$target" "$execs_formatted" "${exec_sec_formatted}/s" "$coverage"

        if [[ " ${FAILED[@]} " =~ " ${target} " ]]; then
            printf "%-10s\n" "✗ 失败"
        elif [ "$crashes" -gt 0 ]; then
            printf "%-10s\n" "⚠ $crashes"
        elif [ "$timeouts" -gt 0 ]; then
            printf "%-10s\n" "⏱ $timeouts"
        else
            printf "%-10s\n" "✓ 通过"
        fi
    done

    echo ""
    echo "───────────────────────────────────────────────────────────────"
    echo "总体统计"
    echo "───────────────────────────────────────────────────────────────"

    if [ "$total_execs" -gt 0 ]; then
        echo "总执行次数: $(printf "%'d" $total_execs)"
    fi
    echo "总corpus大小: $(find "$FUZZ_DIR/corpus" -type f 2>/dev/null | wc -l) 个文件"
    echo "总crash数量: $(find "$FUZZ_DIR/corpus" -name "crash-*" -type f 2>/dev/null | wc -l)"
    echo "总超时数量: $(find "$FUZZ_DIR/corpus" -name "timeout-*" -type f 2>/dev/null | wc -l)"
    echo "总泄漏数量: $(find "$FUZZ_DIR/corpus" -name "leak-*" -type f 2>/dev/null | wc -l)"
    echo "通过率: $(echo "scale=1; (${#FUZZ_TARGETS[@]} - ${#FAILED[@]}) * 100 / ${#FUZZ_TARGETS[@]}" | bc)%"
    echo ""

    if [ ${#FAILED[@]} -gt 0 ]; then
        echo "失败的targets: ${FAILED[*]}"
        echo ""
    fi

    # 关键发现
    total_issues=$(find "$FUZZ_DIR/corpus" -name "crash-*" -type f 2>/dev/null | wc -l)
    total_issues=$((total_issues + $(find "$FUZZ_DIR/corpus" -name "timeout-*" -type f 2>/dev/null | wc -l)))
    total_issues=$((total_issues + $(find "$FUZZ_DIR/corpus" -name "leak-*" -type f 2>/dev/null | wc -l)))

    if [ $total_issues -eq 0 ]; then
        echo "✅ 所有测试通过，未发现问题"
    else
        echo "⚠️  发现 $total_issues 个问题需要关注"
    fi

    echo ""
    echo "详细报告: $MD_REPORT"
    echo "═══════════════════════════════════════════════════════════════"

} > "$SUMMARY_FILE"

# 在终端显示摘要
cat "$SUMMARY_FILE"

echo ""
echo -e "${CYAN}📄 生成的报告文件:${NC}"
echo -e "  ${BOLD}Markdown:${NC} $MD_REPORT"
echo -e "  ${BOLD}纯文本:${NC}  $SUMMARY_FILE"
echo ""

# 检查crash文件
echo -e "${YELLOW}🔍 Crash检测:${NC}"
TOTAL_CRASHES=0
for target in "${FUZZ_TARGETS[@]}"; do
    CRASH_DIR="$FUZZ_DIR/corpus/${target}"
    if [ -d "$CRASH_DIR" ]; then
        CRASH_COUNT=$(find "$CRASH_DIR" -name "crash-*" -type f 2>/dev/null | wc -l)
        if [ "$CRASH_COUNT" -gt 0 ]; then
            echo -e "  ${RED}⚠ $target: $CRASH_COUNT 个crash${NC}"
            TOTAL_CRASHES=$((TOTAL_CRASHES + CRASH_COUNT))
        fi
    fi
done

if [ $TOTAL_CRASHES -eq 0 ]; then
    echo -e "  ${GREEN}✓ 未发现crashes${NC}"
else
    echo -e "  ${RED}总计: $TOTAL_CRASHES 个crash${NC}"
    echo ""
    echo -e "${YELLOW}💡 重现crash命令:${NC}"
    for target in "${FUZZ_TARGETS[@]}"; do
        crash_file=$(find "$FUZZ_DIR/corpus/$target" -name "crash-*" -type f 2>/dev/null | head -1)
        if [ -n "$crash_file" ]; then
            echo -e "  cargo fuzz run $target $crash_file"
        fi
    done
fi

echo ""
echo -e "${BOLD}${GREEN}✓ 测试完成！${NC}"
echo -e "${BLUE}查看完整报告: cat $MD_REPORT${NC}"

# 返回适当的退出码
[ ${#FAILED[@]} -eq 0 ]
