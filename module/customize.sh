SKIPUNZIP=0
check_magisk_version() {
	ui_print "- Magisk version: $MAGISK_VER_CODE"
	ui_print "- Module version: $(grep_prop version "${TMPDIR}/module.prop")"
	ui_print "- Module versionCode: $(grep_prop versionCode "${TMPDIR}/module.prop")"
	ui_print "********************************************"
	ui_print "- $(grep_prop description "${TMPDIR}/module.prop")"
	if [ "$MAGISK_VER_CODE" -lt 20400 ]; then
		ui_print "********************************************"
		ui_print "! 请安装 Magisk v20.4+ (20400+)"
		abort    "********************************************"
	fi
}
check_required_files() {
	REQUIRED_FILE_LIST="/sys/devices/system/cpu/present /proc/loadavg"
	for REQUIRED_FILE in $REQUIRED_FILE_LIST; do
		if [ ! -e $REQUIRED_FILE ]; then
			ui_print "********************************************"
			ui_print "! $REQUIRED_FILE 文件不存在"
			ui_print "! 请联系模块作者"
			abort    "********************************************"
		fi
	done
}
extract_bin() {
	ui_print "********************************************"
	if [ "$ARCH" == "arm64" ]; then
		cp $MODPATH/bin/arm64-v8a/AppOpt* $MODPATH
	elif [ "$ARCH" == "x64" ]; then
		cp $MODPATH/bin/x86_64/AppOpt* $MODPATH
	else
		abort "! Unsupported platform: $ARCH"
	fi
	ui_print "- Device platform: $ARCH"
	rm -rf $MODPATH/bin
	[ -f $MODPATH/AppOpt ] && chmod a+x $MODPATH/AppOpt
	if ! $MODPATH/AppOpt -v; then
		abort "! 主程序验证失败，请检查模块zip文件是否损坏"
	fi
}

format_cpu_ranges() {
	[ -z "${1// /}" ] && { cat /sys/devices/system/cpu/present; return; }
	awk -v input="$1" 'BEGIN {
		n = split(input, arr, /[[:space:]]+/)
		j = 0
		for (i = 1; i <= n; i++) {
			if (arr[i] != "" && !seen[arr[i]]++) 
				nums[++j] = arr[i] + 0
		}
		n = j
		if (!n) exit
		for (i = 1; i < n; i++) {
			min = i
			for (j = i + 1; j <= n; j++)
				if (nums[j] < nums[min]) min = j
			if (min != i) {
				t = nums[i]
				nums[i] = nums[min]
				nums[min] = t
			}
		}
		start = last = nums[1]
		for (i = 2; i <= n; i++) {
			if (nums[i] == last + 1) {
				last = nums[i]
				continue
			}
			printf "%s%s", sep, (start == last ? start : start "-" last)
			sep = ","
			start = last = nums[i]
		}
		printf "%s", sep
		printf (start == last ? start : start "-" last)
	}'
}
# 每个 cpufreq 策略一行 "最高频率:核心:相对性能"，同频率合并；相对性能（cpu_capacity）任一缺失即留空
sorted_groups=$(
	for policy in /sys/devices/system/cpu/cpufreq/policy*; do
		[ -d "$policy" ] || continue
		cpus=$(cat "$policy/related_cpus" 2>/dev/null)
		freq=$(cat "$policy/cpuinfo_max_freq" 2>/dev/null)
		[ -z "$cpus" ] || [ -z "$freq" ] && continue
		cap=$(cat "/sys/devices/system/cpu/cpu${cpus%% *}/cpu_capacity" 2>/dev/null)
		echo "$freq:$cpus:$cap"
	done | sort -n -t: -k1,1 | awk -F: '
	$1 == prev { cores = cores " " $2; if ($3 == "" || cap == "") cap = ""; else if ($3 + 0 > cap + 0) cap = $3; next }
	prev != "" { print prev ":" cores ":" cap }
	{ prev = $1; cores = $2; cap = $3 }
	END { if (prev != "") print prev ":" cores ":" cap }'
)
# 与主程序一致：最高一档为大核；其余各档相对性能不到最强一档一半的是小核，其余是中核；
# 没有小核时 e_core 取最低一档；相对性能缺失或不可信时最低一档为小核
eval "$(echo "$sorted_groups" | awk -F: '
NF { n++; freq[n] = $1; cpus[n] = $2; cap[n] = $3 }
END {
	e = ""; p = ""; hp = ""; ef = 0; pf = 0; hf = 0; little = 0
	if (n == 1) { e = cpus[1]; ef = freq[1] }
	if (n >= 2) {
		hp = cpus[n]; hf = freq[n]
		usable = (cap[n] != "")
		for (i = 1; i < n; i++) if (cap[i] == "" || cap[i] + 0 >= cap[n] + 0) usable = 0
		for (i = 1; i < n; i++) {
			if (usable ? (cap[i] * 2 < cap[n] + 0) : (i == 1)) {
				e = e (e == "" ? "" : " ") cpus[i]; if (freq[i] > ef) ef = freq[i]; little = 1
			} else {
				p = p (p == "" ? "" : " ") cpus[i]; if (freq[i] > pf) pf = freq[i]
			}
		}
		if (!little) { e = cpus[1]; ef = freq[1] }
	}
	printf "e_core=\"%s\"; e_core_freq=%d; p_core=\"%s\"; p_core_freq=%d; ", e, ef, p, pf
	printf "hp_core=\"%s\"; hp_core_freq=%d; has_little=%d;", hp, hf, little
}')"
all_core="$(cat /sys/devices/system/cpu/present)"
module_instructions() {
	ui_print "********************************************"
	ui_print "线程规则配置文件路径为："
	ui_print "/data/adb/modules/AppOpt/applist.conf"
	ui_print "------------------------------------------"
	ui_print "修改与添加规则无需重启，即时生效"
	ui_print "********************************************"
	cores=$(for cpus in /sys/devices/system/cpu/cpufreq/*/related_cpus; do 
		[ -f "$cpus" ] && cat "$cpus" | wc -w
	done | paste -sd+)
	ui_print "当前$(getprop ro.soc.model)设备为$(nproc)核CPU，规格是：$cores"
	ui_print "可用CPU范围：$all_core"
	ui_print "------------------------------------------"
	[ "$has_little" = 1 ] && echo "$(format_cpu_ranges "$e_core") 为能效小核，频率最高$((e_core_freq/1000)) MHz"
	[ -n "$p_core" ] && echo "$(format_cpu_ranges "$p_core") 为性能中核，频率最高$((p_core_freq/1000)) MHz"
	[ -n "$hp_core" ] && echo "$(format_cpu_ranges "$hp_core") 为高性能大核，频率最高$((hp_core_freq/1000)) MHz"
	if [ "$has_little" = 1 ]; then
		low_name="能效小核"
	else
		low_name="最省电的一组核心"
		[ -n "$hp_core" ] && echo "这颗芯片没有能效小核，规则里的 e-core 指 $(format_cpu_ranges "$e_core")"
	fi
	ui_print "------------------------------------------"
	ui_print "applist.conf 规则写法示例："
	ui_print "------------------------------------------"
	if [ -n "$all_core" ]; then
		ui_print "允许安卓 '图形显示组件 '调用所有核心："
		ui_print "surfaceflinger=$all_core"
		ui_print "------------------------------------------"
	fi
	if [ -n "$hp_core" ]; then
		ui_print "单独将'图形显示组件'渲染引擎线程绑定到大核："
		ui_print "surfaceflinger{RenderEngine}=$(format_cpu_ranges "$hp_core")"
		ui_print "------------------------------------------"
	fi
	if [ -n "$hp_core" ]; then
		ui_print "将 '微信' 主进程绑定${low_name}："
		ui_print "com.tencent.mm=$(format_cpu_ranges "$e_core")"
		ui_print "------------------------------------------"
	fi
	if [ -n "$hp_core" ]; then
		ui_print "将 '微信' 消息推送子进程绑定${low_name}："
		ui_print "com.tencent.mm:push=$(format_cpu_ranges "$e_core")"
		ui_print "------------------------------------------"
	fi
	if [ -n "$hp_core" ]; then
		ui_print "将 '系统界面' 渲染线程绑定到性能大核："
		ui_print "com.android.systemui{RenderThread}=$(format_cpu_ranges "$hp_core")"
		ui_print "------------------------------------------"
	fi
	ui_print "更多规则使用说明请参考："
	ui_print "http://AppOpt.suto.top"
	ui_print "------------------------------------------"
	ui_print "Web 管理面板："
	ui_print "http://127.0.0.1:8889"
	ui_print "********************************************"
}
copy_default_rules() {
if [ -f /data/adb/modules/AppOpt/applist.conf ]; then
	mv $MODPATH/applist.conf $MODPATH/applist.conf.bak
	cp -r /data/adb/modules/AppOpt/applist.conf $MODPATH
fi
if [ -f /data/adb/modules/AppOpt/AppOpt.json ]; then
	cp -r /data/adb/modules/AppOpt/AppOpt.json $MODPATH
fi
}
check_magisk_version
check_required_files
copy_default_rules
extract_bin
module_instructions
set_perm_recursive "$MODPATH" 0 0 0755 0644
set_perm_recursive "$MODPATH/*.sh $MODPATH/AppOpt $MODPATH/AppOpt-ebpf" 0 2000 0755 0755 u:object_r:magisk_file:s0
