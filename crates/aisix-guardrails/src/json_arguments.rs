//! 工具参数与拒绝内容导出共用的 JSON 解码边界。

// 保留 serde_json 的默认递归限额；每个对象或数组消耗一层，不能解除它来绕过检查。
const JSON_DECODE_DEPTH_LIMIT: usize = 128;

/// 解析失败后，判断字符串是否超过 serde_json 的默认嵌套限额。
/// 错误分类不能区分深度溢出和普通语法错误，因此只按结构字节计数；
/// 字符串中的括号和转义不计入结构深度，不再尝试递归解析。
pub fn nests_past_decode_limit(encoded: &str) -> bool {
    let mut depth: usize = 0;
    let mut escaped = false;
    let mut in_string = false;
    for byte in encoded.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' | b'[' if !in_string => {
                depth += 1;
                if depth >= JSON_DECODE_DEPTH_LIMIT {
                    return true;
                }
            }
            b'}' | b']' if !in_string => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}
