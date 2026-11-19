//! Кураторский каталог локальных моделей.
//!
//! Правило: пользователь видит модели не крупнее своего железа, но лёгкие
//! доступны и на сильном. Каждая запись — данные: размеры сверены по манифестам
//! реестра Ollama и Hugging Face 08.11.2026, SHA-256 нужен проверке GGUF.
//! Установка всегда создаёт производную модель `brepsi/<id>` с фиксированным
//! окном контекста, поэтому каталог не зависит от того, как модель названа
//! в реестре.

use crate::config::Task;
use crate::hardware::{Backend, Hardware, Tier};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Тег реестра Ollama: тянется `POST /api/pull`.
    Ollama(&'static str),
    /// Одиночный GGUF из Hugging Face: Range-докачка и проверка SHA-256.
    Hf { repo: &'static str, file: &'static str, sha256: &'static str, size: u64 },
}

#[derive(Debug, Clone, Copy)]
pub struct Model {
    pub id: &'static str,
    pub name: &'static str,
    pub params: &'static str,
    pub quant: &'static str,
    pub size_bytes: u64,
    /// Минимум видеопамяти, чтобы модель и KV-кэш поместились. На процессоре
    /// ограничение другое: доступны только модели тира `minimal`.
    pub min_vram_mb: u64,
    pub min_tier: Tier,
    pub roles: &'static [Task],
    /// Языки, на которых модель держит качество: «ru,en» или «en».
    pub languages: &'static str,
    pub summary_ru: &'static str,
    pub summary_en: &'static str,
    /// Для каких тиров модель рекомендуем, когда её железо подходит.
    pub recommended_for: &'static [Tier],
    pub source: Source,
}

const ALL: &[Task] = &[Task::Distill, Task::Answer, Task::Handoff, Task::Projects];
const TEXT: &[Task] = &[Task::Distill, Task::Answer, Task::Projects];

const MODELS: &[Model] = &[
    Model {
        id: "gemma3-1b",
        name: "Gemma 3 1B",
        params: "1B",
        quant: "Q4_K_M",
        size_bytes: 806_058_400,
        min_vram_mb: 0,
        min_tier: Tier::Minimal,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Самая лёгкая: считается на процессоре, отвечает быстро, глубина ниже крупных.",
        summary_en: "The lightest model: runs on a CPU, answers fast, less depth than the bigger ones.",
        recommended_for: &[Tier::Minimal],
        source: Source::Ollama("gemma3:1b"),
    },
    Model {
        id: "qwen25-15b",
        name: "Qwen2.5 1.5B Instruct",
        params: "1.5B",
        quant: "Q4_K_M",
        size_bytes: 986_048_512,
        min_vram_mb: 0,
        min_tier: Tier::Minimal,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Крошечная модель для слабых машин и процессора: короткие карточки без долгих ожиданий.",
        summary_en: "Tiny model for weak machines and CPUs: short cards without long waits.",
        recommended_for: &[],
        source: Source::Ollama("qwen2.5:1.5b"),
    },
    Model {
        id: "qwen25-3b",
        name: "Qwen2.5 3B Instruct",
        params: "3B",
        quant: "Q4_K_M",
        size_bytes: 1_929_903_008,
        min_vram_mb: 4_000,
        min_tier: Tier::Minimal,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Разумный минимум: уверенный русский текст и JSON, помещается на 4 ГБ или в 8 ГБ RAM.",
        summary_en: "A reasonable minimum: confident Russian and JSON, fits in 4 GB VRAM or 8 GB RAM.",
        recommended_for: &[],
        source: Source::Ollama("qwen2.5:3b"),
    },
    Model {
        id: "gemma3-4b",
        name: "Gemma 3 4B",
        params: "4B",
        quant: "Q4_K_M",
        size_bytes: 3_350_263_392,
        min_vram_mb: 4_000,
        min_tier: Tier::Minimal,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Быстрая мультиязычная модель на 4+ ГБ; видит картинки, но здесь используется только текст.",
        summary_en: "Fast multilingual model for 4+ GB; it can see images, but only text is used here.",
        recommended_for: &[],
        source: Source::Ollama("gemma3:4b"),
    },
    Model {
        id: "phi4-mini",
        name: "Phi-4 Mini",
        params: "3.8B",
        quant: "Q4_K_M",
        size_bytes: 2_491_874_624,
        min_vram_mb: 3_000,
        min_tier: Tier::Minimal,
        roles: TEXT,
        languages: "en",
        summary_ru: "Сильна в рассуждениях и JSON, но русский слабее: годится для карточек на английском.",
        summary_en: "Strong at reasoning and JSON, weaker Russian: fits English-language cards.",
        recommended_for: &[],
        source: Source::Ollama("phi4-mini:latest"),
    },
    Model {
        id: "qwen25-7b",
        name: "Qwen2.5 7B Instruct",
        params: "7B",
        quant: "Q4_K_M",
        size_bytes: 4_683_073_952,
        min_vram_mb: 7_000,
        min_tier: Tier::Low,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Лучший баланс для 8 ГБ: аккуратные карточки, атомы и ответы по-русски.",
        summary_en: "The best balance for 8 GB: careful cards, atoms and answers in Russian.",
        recommended_for: &[Tier::Low],
        source: Source::Ollama("qwen2.5:7b"),
    },
    Model {
        id: "qwen25-7b-hf",
        name: "Qwen2.5 7B Instruct (Hugging Face)",
        params: "7B",
        quant: "Q4_K_M",
        size_bytes: 4_683_074_240,
        min_vram_mb: 7_000,
        min_tier: Tier::Low,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Та же модель с Hugging Face: запасной путь, если реестр Ollama недоступен.",
        summary_en: "The same model from Hugging Face: a fallback when the Ollama registry is blocked.",
        recommended_for: &[],
        source: Source::Hf {
            repo: "bartowski/Qwen2.5-7B-Instruct-GGUF",
            file: "Qwen2.5-7B-Instruct-Q4_K_M.gguf",
            sha256: "65b8fcd92af6b4fefa935c625d1ac27ea29dcb6ee14589c55a8f115ceaaa1423",
            size: 4_683_074_240,
        },
    },
    Model {
        id: "qwen25-3b-hf",
        name: "Qwen2.5 3B Instruct (Hugging Face)",
        params: "3B",
        quant: "Q4_K_M",
        size_bytes: 1_929_903_264,
        min_vram_mb: 4_000,
        min_tier: Tier::Minimal,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Лёгкая модель с Hugging Face: слабые машины без доступа к реестру Ollama.",
        summary_en: "A light model from Hugging Face: weak machines without registry access.",
        recommended_for: &[],
        source: Source::Hf {
            repo: "bartowski/Qwen2.5-3B-Instruct-GGUF",
            file: "Qwen2.5-3B-Instruct-Q4_K_M.gguf",
            sha256: "9c9f56a391a3abbd5b89d0245bf6106081bcc3173119d4229235dd9d23253f94",
            size: 1_929_903_264,
        },
    },
    Model {
        id: "llama31-8b",
        name: "Llama 3.1 8B Instruct",
        params: "8B",
        quant: "Q4_K_M",
        size_bytes: 4_920_738_944,
        min_vram_mb: 7_000,
        min_tier: Tier::Low,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Ровная модель общего назначения; русский держит слабее Qwen, английский — уверенно.",
        summary_en: "A steady general-purpose model; Russian is weaker than Qwen, English is solid.",
        recommended_for: &[],
        source: Source::Ollama("llama3.1:8b"),
    },
    Model {
        id: "mistral-nemo-12b",
        name: "Mistral Nemo 12B",
        params: "12B",
        quant: "Q4_K_M",
        size_bytes: 7_071_700_672,
        min_vram_mb: 11_000,
        min_tier: Tier::Mid,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "12B с длинным контекстом: для карточек и ответов на 11–15 ГБ VRAM.",
        summary_en: "12B with a long context: cards and answers on 11–15 GB of VRAM.",
        recommended_for: &[],
        source: Source::Ollama("mistral-nemo:12b"),
    },
    Model {
        id: "gemma3-12b",
        name: "Gemma 3 12B",
        params: "12B",
        quant: "Q4_K_M",
        size_bytes: 8_164_030_720,
        min_vram_mb: 11_000,
        min_tier: Tier::Mid,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Мультиязычная 12B с крепким русским; на 11 ГБ идёт с небольшим окном контекста.",
        summary_en: "A multilingual 12B with solid Russian; on 11 GB use a smaller context window.",
        recommended_for: &[],
        source: Source::Ollama("gemma3:12b"),
    },
    Model {
        id: "qwen25-14b",
        name: "Qwen2.5 14B Instruct",
        params: "14B",
        quant: "Q4_K_M",
        size_bytes: 8_988_110_688,
        min_vram_mb: 11_000,
        min_tier: Tier::Mid,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Лучшее качество карточек и ответов на 11–15 ГБ; заметно точнее 7B.",
        summary_en: "The best card and answer quality on 11–15 GB; noticeably sharper than 7B.",
        recommended_for: &[Tier::Mid],
        source: Source::Ollama("qwen2.5:14b"),
    },
    Model {
        id: "qwen25-14b-hf",
        name: "Qwen2.5 14B Instruct (Hugging Face)",
        params: "14B",
        quant: "Q4_K_M",
        size_bytes: 8_988_110_976,
        min_vram_mb: 11_000,
        min_tier: Tier::Mid,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "14B с Hugging Face: запасной источник для 11–15 ГБ.",
        summary_en: "14B from Hugging Face: a fallback source for 11–15 GB.",
        recommended_for: &[],
        source: Source::Hf {
            repo: "bartowski/Qwen2.5-14B-Instruct-GGUF",
            file: "Qwen2.5-14B-Instruct-Q4_K_M.gguf",
            sha256: "e47ad95dad6ff848b431053b375adb5d39321290ea2c638682577dafca87c008",
            size: 8_988_110_976,
        },
    },
    Model {
        id: "gemma3-27b",
        name: "Gemma 3 27B",
        params: "27B",
        quant: "Q4_K_M",
        size_bytes: 17_413_400_640,
        min_vram_mb: 20_000,
        min_tier: Tier::High,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "27B для 24 ГБ: глубокие карточки, но и загрузка дольше, и VRAM много.",
        summary_en: "27B for 24 GB: deeper cards, but longer loading and lots of VRAM.",
        recommended_for: &[],
        source: Source::Ollama("gemma3:27b"),
    },
    Model {
        id: "qwen25-32b",
        name: "Qwen2.5 32B Instruct",
        params: "32B",
        quant: "Q4_K_M",
        size_bytes: 19_851_336_288,
        min_vram_mb: 24_000,
        min_tier: Tier::High,
        roles: ALL,
        languages: "ru,en",
        summary_ru: "Флагман каталога для 24 ГБ: самые полные карточки и точные ответы.",
        summary_en: "The catalog flagship for 24 GB: the fullest cards and sharpest answers.",
        recommended_for: &[Tier::High],
        source: Source::Ollama("qwen2.5:32b"),
    },
];

pub fn models() -> &'static [Model] {
    MODELS
}

pub fn find(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|model| model.id == id)
}

/// Помещается ли модель с её KV-кэшем: на GPU решает фактический объём VRAM,
/// на процессоре доступны только лёгкие модели.
pub fn fits(model: &Model, hardware: &Hardware) -> bool {
    match hardware.backend {
        Backend::Cpu => model.min_tier == Tier::Minimal,
        _ => hardware.vram_mb.unwrap_or(0) >= model.min_vram_mb,
    }
}

pub fn visible_for(hardware: &Hardware) -> Vec<&'static Model> {
    MODELS.iter().filter(|model| fits(model, hardware)).collect()
}

/// Рекомендуемая: отмеченная для тира среди подходящих по VRAM, иначе самая
/// крупная из подходящих. Тир сам по себе не гарантирует, что модель влезет.
pub fn recommended_for(hardware: &Hardware) -> &'static Model {
    let tier = hardware.tier;
    let visible = visible_for(hardware);
    visible
        .iter()
        .copied()
        .filter(|model| model.recommended_for.contains(&tier))
        .max_by_key(|model| model.size_bytes)
        // Запасной путь: самая крупная из основного источника (Ollama), а
        // Hugging Face — только если реестрных вариантов нет.
        .or_else(|| {
            visible
                .iter()
                .copied()
                .filter(|model| matches!(model.source, Source::Ollama(_)))
                .max_by_key(|model| model.size_bytes)
        })
        .or_else(|| visible.iter().copied().max_by_key(|model| model.size_bytes))
        .unwrap_or(&MODELS[0])
}

/// Имя производной модели в хранилище Ollama.
pub fn tag(id: &str) -> String {
    format!("brepsi/{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{build, Vendor};

    fn gpu(vendor: Vendor, vram_mb: u64) -> Hardware {
        build(vendor, "test gpu".into(), vram_mb, 32_768)
    }

    fn cpu() -> Hardware {
        Hardware {
            vendor: Vendor::Unknown,
            name: None,
            vram_mb: None,
            ram_mb: 16_384,
            backend: Backend::Cpu,
            tier: Tier::Minimal,
            notes: Vec::new(),
        }
    }

    #[test]
    fn ids_are_unique_and_sizes_positive() {
        let mut ids: Vec<&str> = MODELS.iter().map(|model| model.id).collect();
        ids.sort();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
        assert!(MODELS.iter().all(|model| model.size_bytes > 0));
    }

    #[test]
    fn hf_sources_carry_sha256() {
        for model in MODELS {
            if let Source::Hf { sha256, size, .. } = model.source {
                assert_eq!(sha256.len(), 64, "{}", model.id);
                assert!(sha256.chars().all(|ch| ch.is_ascii_hexdigit()), "{}", model.id);
                assert_eq!(size, model.size_bytes, "{}", model.id);
            }
        }
    }

    #[test]
    fn rx580_sees_low_models_and_recommends_qwen() {
        let hardware = gpu(Vendor::Amd, 8_192);
        assert_eq!(hardware.tier, Tier::Low);
        let visible = visible_for(&hardware);
        assert!(visible.iter().any(|model| model.id == "qwen25-7b"));
        assert!(visible.iter().any(|model| model.id == "gemma3-1b"));
        assert!(!visible.iter().any(|model| model.id == "qwen25-14b"));
        assert_eq!(recommended_for(&hardware).id, "qwen25-7b");
    }

    #[test]
    fn cpu_gets_minimal_recommendation() {
        let hardware = cpu();
        assert_eq!(recommended_for(&hardware).id, "gemma3-1b");
        assert!(visible_for(&hardware).iter().all(|model| model.min_tier == Tier::Minimal));
    }

    #[test]
    fn sixteen_gb_does_not_recommend_a_model_that_cannot_fit() {
        let hardware = gpu(Vendor::Nvidia, 16_303);
        assert_eq!(hardware.tier, Tier::High);
        assert!(!fits(find("qwen25-32b").unwrap(), &hardware));
        assert!(!fits(find("gemma3-27b").unwrap(), &hardware));
        assert_eq!(recommended_for(&hardware).id, "qwen25-14b");
    }

    #[test]
    fn twenty_four_gb_still_recommends_the_flagship() {
        let hardware = gpu(Vendor::Nvidia, 24_576);
        assert!(fits(find("qwen25-32b").unwrap(), &hardware));
        assert_eq!(recommended_for(&hardware).id, "qwen25-32b");
    }

    #[test]
    fn tags_are_namespaced() {
        assert_eq!(tag("qwen25-7b"), "brepsi/qwen25-7b");
    }
}
