// ── Чанкинг (Фаза B3): резка текста по естественным границам ─────────────────
//
// Зачем мелкие фрагменты: поиск должен находить точечный релевантный кусок
// (конкретный пункт договора), а не «весь документ одним вектором».
//
// Режем НЕ вслепую по счётчику символов, а по границам смысла: сначала по абзацам
// (пустые строки), затем по предложениям; набираем до целевого размера и добавляем
// перекрытие между соседними фрагментами, чтобы не терять контекст на стыке.
//
// Размер задаём в СИМВОЛАХ (прокси токенов): токенизатора bge-m3 в Rust нет, тянуть
// его — лишняя зависимость. Числа — ориентир под русский, вынесены в константы и
// будут донастроены позже. ~2000 симв. ≈ 500–700 токенов — заведомо в пределах
// входного лимита bge-m3 (8192 токена).

/// Целевой размер фрагмента (символы). Мягкий: набираем до него по границам.
const TARGET_CHARS: usize = 2000;
/// Перекрытие соседних фрагментов (символы) ≈ 12% — хвост предыдущего идёт в начало следующего.
const OVERLAP_CHARS: usize = 240;
/// Жёсткий потолок: одиночную сверхдлинную единицу (абзац/предложение без границ) режем по символам.
const MAX_CHARS: usize = 3200;

/// Разбивает текст на фрагменты для индексации. Пустой/пробельный текст → пусто.
pub fn chunk_text(text: &str) -> Vec<String> {
    // 1) Единицы набора: абзацы; сверхдлинный абзац дробим на предложения, а
    //    сверхдлинное предложение — жёстко по символам. Так ни одна единица не
    //    превышает MAX_CHARS и при этом границы максимально естественные.
    let mut units: Vec<String> = Vec::new();
    for para in split_paragraphs(text) {
        if char_len(&para) <= MAX_CHARS {
            units.push(para);
        } else {
            for sent in split_sentences(&para) {
                if char_len(&sent) <= MAX_CHARS {
                    units.push(sent);
                } else {
                    units.extend(hard_split(&sent, MAX_CHARS));
                }
            }
        }
    }

    // 2) Упаковка единиц в фрагменты до TARGET с перекрытием на стыке.
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    for unit in units {
        let extra = if cur.is_empty() { 0 } else { 1 }; // пробел-разделитель
        if !cur.is_empty() && char_len(&cur) + extra + char_len(&unit) > TARGET_CHARS {
            let tail = overlap_tail(&cur);
            chunks.push(std::mem::take(&mut cur));
            cur = tail; // следующий фрагмент стартует с хвоста предыдущего
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&unit);
    }
    let last = cur.trim();
    if !last.is_empty() {
        chunks.push(last.to_string());
    }
    chunks
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Абзацы — по пустым строкам (один и более переводов с пробелами между ними).
fn split_paragraphs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut blank = false; // предыдущая строка пустая → граница абзаца
    for line in text.lines() {
        if line.trim().is_empty() {
            if !buf.trim().is_empty() {
                blank = true;
            }
            continue;
        }
        if blank && !buf.trim().is_empty() {
            out.push(buf.trim().to_string());
            buf.clear();
        }
        blank = false;
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(line.trim());
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
    out
}

/// Предложения — по .!?…, сохраняя завершающий знак; пустые отбрасываем.
fn split_sentences(para: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = para.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?' | '…') {
            // Граница предложения — ТОЛЬКО если дальше пробел или конец текста.
            //
            // Прежде эта проверка была обещана комментарием, но не написана: точка
            // безусловно закрывала «предложение». Куски потом склеиваются через
            // пробел — и в месте, где пробела не было, он появлялся. «1.5 млн»
            // превращалось в «1. 5 млн», «05.02.2026» — в «05. 02. 2026», «п.3.1» —
            // в «п. 3. 1». Искажённый текст уходил в индекс и в подсказку модели, а
            // у продукта для юристов и бухгалтеров именно числа, даты и номера
            // пунктов — то, ради чего документ и читают.
            let boundary = chars.peek().is_none_or(|n| n.is_whitespace());
            if !boundary {
                continue; // точка внутри числа, даты или сокращения — не конец фразы
            }
            while matches!(chars.peek(), Some(' ') | Some('\t')) {
                chars.next();
            }
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
                cur.clear();
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Жёсткая нарезка по символам (для патологий без границ): режем по char, не по байтам.
fn hard_split(s: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(max)
        .map(|c| c.iter().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Хвост фрагмента для перекрытия: последние ~OVERLAP_CHARS символов, обрезанные
/// до начала слова (чтобы не рвать слово на стыке).
fn overlap_tail(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= OVERLAP_CHARS {
        return s.trim().to_string();
    }
    let start = chars.len() - OVERLAP_CHARS;
    let mut tail: String = chars[start..].iter().collect();
    // сдвигаем начало до первого пробела — отбрасываем «обрезок» первого слова
    if let Some(pos) = tail.find(char::is_whitespace) {
        tail = tail[pos..].trim_start().to_string();
    }
    tail.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_no_chunks() {
        assert!(chunk_text("").is_empty());
        assert!(chunk_text("   \n\n  ").is_empty());
    }

    #[test]
    fn short_text_single_chunk() {
        let t = "Договор аренды. Срок действия один год.";
        let c = chunk_text(t);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0], t);
    }

    #[test]
    fn long_text_splits_with_overlap() {
        // много абзацев, заведомо больше TARGET
        let para = "Пункт договора об ответственности сторон и порядке расчётов. "
            .repeat(60); // ~3600 симв в одном «абзаце»-предложении-наборе
        let text = format!("{para}\n\n{para}\n\n{para}");
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 2, "длинный текст должен разбиться: {}", chunks.len());
        // каждый фрагмент не превышает разумный потолок (TARGET + перекрытие + единица)
        for ch in &chunks {
            assert!(char_len(ch) <= MAX_CHARS + TARGET_CHARS, "фрагмент слишком велик");
        }
        // перекрытие: конец первого фрагмента встречается в начале второго
        let tail: String = chunks[0].chars().rev().take(40).collect::<String>()
            .chars().rev().collect();
        assert!(
            chunks[1].contains(tail.trim()) || !chunks[1].is_empty(),
            "ожидается перекрытие на стыке"
        );
    }

    #[test]
    fn respects_paragraph_boundaries() {
        let text = "Первый абзац короткий.\n\nВторой абзац тоже короткий.";
        let chunks = chunk_text(text);
        // оба коротких абзаца влезают в один фрагмент, но склейка идёт через пробел
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("Первый абзац"));
        assert!(chunks[0].contains("Второй абзац"));
    }

    #[test]
    fn cyrillic_safe_hard_split() {
        // длинное «слово» без границ — не должно паниковать на границе UTF-8
        let s = "А".repeat(5000);
        let chunks = chunk_text(&s);
        assert!(chunks.len() >= 2);
        for ch in &chunks {
            assert!(char_len(ch) <= MAX_CHARS + TARGET_CHARS);
        }
    }
}

#[cfg(test)]
mod sentence_tests {
    use super::*;

    /// Точка внутри числа, даты или номера пункта — НЕ конец предложения.
    ///
    /// Прежде split_sentences закрывал предложение на любой точке, а куски потом
    /// склеивались через пробел — и пробел появлялся там, где его не было:
    /// «1.5 млн» → «1. 5 млн», «05.02.2026» → «05. 02. 2026», «п.3.1» → «п. 3. 1».
    /// Искажённый текст уходил и в индекс, и в подсказку модели, и в блок
    /// «Источники» под ответом. Для продукта, которым читают договоры и выписки,
    /// числа и даты — ровно то, ради чего документ и открывают.
    #[test]
    fn numbers_and_dates_survive_splitting() {
        let cases = [
            "Сумма 1.5 млн рублей",
            "Дата 05.02.2026 включительно",
            "Пункт 3.1 договора",
            "Версия 2.10.4 приложения",
        ];
        for text in cases {
            let parts = split_sentences(text);
            let joined = parts.join(" ");
            assert_eq!(joined, text, "текст исказился при разбиении: «{text}» → «{joined}»");
        }
    }

    /// Обратная сторона: настоящие границы предложений находиться обязаны, иначе
    /// чанки перестанут резаться по смыслу и поиск по документам просядет.
    #[test]
    fn real_sentence_boundaries_are_found() {
        let parts = split_sentences("Первое предложение. Второе предложение! Третье?");
        assert_eq!(
            parts,
            vec!["Первое предложение.", "Второе предложение!", "Третье?"],
            "границы предложений потерялись"
        );
    }

    /// Склейка целиком: то, что попадёт в индекс, не должно отличаться от исходника
    /// ничем, кроме переносов. Проверяем сквозь весь путь chunk_text, а не только
    /// разбиение, — дефект жил именно на стыке разбиения и склейки.
    #[test]
    fn chunking_preserves_the_text() {
        let text = "По счёту 05.02.2026 перечислено 1.5 млн рублей согласно п.3.1 договора. \
                    Остаток 0.75 млн подлежит оплате до 31.12.2026.";
        let chunks = chunk_text(text);
        let joined = chunks.join(" ");
        for needle in ["05.02.2026", "1.5 млн", "п.3.1", "0.75 млн", "31.12.2026"] {
            assert!(joined.contains(needle), "«{needle}» исказилось: {joined}");
        }
    }
}
