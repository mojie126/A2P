use agent2api_provider_sdk::settings::{Field, Group, Schema};
use serde_json::{json, Value};

use super::claim;

fn translated(values: [&str; 6]) -> Value {
    json!(std::collections::BTreeMap::from_iter(
        ["zh-CN", "en", "ja", "ko", "pt-BR", "zh-Hant"].into_iter().zip(values),
    ))
}

pub fn group() -> Group { Group::new("ZCode") }

/// 「参与每日签到」开关的键名（`plugin.zcode.dailyCheckin`）。
///
/// 缺省（没有这一格）= 参与，与网关 `auto_checkin::daily_checkin_enabled`
/// 的读取口径一致，两个地区共用同一份设置
pub const SETTING_DAILY_CHECKIN: &str = "dailyCheckin";

pub fn schema() -> Schema {
    let mut checkin = Field::new(SETTING_DAILY_CHECKIN, translated([
        "参与每日签到", "Daily check-in", "毎日チェックイン", "매일 출석", "Check-in diário", "參與每日簽到",
    ]), "boolean", json!(true));
    checkin.description = translated([
        "关闭后不再自动领取每日赠送套餐，活动套餐仍可在活动福利里手动领取。国内版与国际版共用此开关。",
        "When off, daily gift plans are no longer claimed automatically; you can still claim offer plans manually. Shared by China and Global editions.",
        "オフにすると毎日の特典プランを自動受け取りしません。特典は特典一覧から手動で受け取れます。中国版と国際版で共有します。",
        "끄면 매일 증정 요금제를 자동으로 받지 않습니다. 혜택 목록에서 직접 받을 수 있습니다. 중국판과 국제판이 공유합니다.",
        "Quando desativado, os planos de brinde diários não são resgatados automaticamente; ainda é possível resgatá-los manualmente nas ofertas. Compartilhado pelas edições China e Global.",
        "關閉後不再自動領取每日贈送方案，活動方案仍可在活動福利裡手動領取。國內版與國際版共用此開關。",
    ]);
    let mut field = Field::new(claim::SETTING_APP_VERSION, translated([
        "客户端版本号", "Client version", "クライアントのバージョン", "클라이언트 버전", "Versão do cliente", "用戶端版本號",
    ]), "string", json!(claim::DEFAULT_APP_VERSION));
    field.trim = true;
    field.empty_is_default = true;
    field.max_length = Some(32);
    field.pattern = Some(r"\A[0-9][0-9A-Za-z._-]*\z".into());
    field.description = translated([
        "仅在官方客户端能看到活动、这里却没有时，填写官方客户端的实际版本。留空使用插件默认值，保存后刷新套餐。最多 32 个字符，以数字开头，只含字母、数字和 . _ -。",
        "Only change this if an offer appears in the official client but not here. Enter that client's actual version, or leave blank for the plugin default. Refresh plans after saving. Use up to 32 letters, digits, dots, underscores or hyphens, starting with a digit.",
        "公式クライアントに表示される特典がここにない場合のみ、その実際のバージョンを入力してください。空欄ではプラグインの既定値を使います。保存後にプランを更新してください。数字で始まる32文字以内の英数字と . _ - が使えます。",
        "공식 클라이언트에는 보이는 혜택이 여기에 없을 때만 실제 버전을 입력하세요. 비워 두면 플러그인 기본값을 사용합니다. 저장 후 요금제를 새로 고치세요. 숫자로 시작하는 최대 32자의 영문, 숫자 및 . _ - 를 사용할 수 있습니다.",
        "Altere apenas se uma oferta aparecer no cliente oficial, mas não aqui. Informe a versão real desse cliente ou deixe em branco para usar o padrão do plugin. Atualize os planos após salvar. Use até 32 letras, números, pontos, sublinhados ou hífens, começando por um número.",
        "僅在官方用戶端能看到活動、這裡卻沒有時，填寫官方用戶端的實際版本。留空使用外掛預設值，儲存後重新整理方案。最多 32 個字元，以數字開頭，只含字母、數字和 . _ -。",
    ]);
    let mut schema = Schema::new(translated([
        "活动与套餐", "Offers and plans", "特典とプラン", "혜택 및 요금제", "Ofertas e planos", "活動與方案",
    ]), vec![checkin, field]);
    schema.storage_provider = Some(claim::APP_VERSION_SETTING_OWNER.into());
    schema.runtime_overrides = true;
    schema.description = translated([
        "签到开关与活动查询使用；客户端版本号不影响套餐池推理使用的版本。国内版与国际版共用这两项；环境变量 ZCODE_APP_VERSION 优先于手动设置。",
        "Used for check-in and offer discovery; the client version does not change the Start Plan inference version. Both settings are shared by China and Global editions; ZCODE_APP_VERSION overrides manual settings.",
        "チェックインと特典の検索に使います。クライアントのバージョンは Start Plan 推論のバージョンには影響しません。中国版と国際版で共有し、環境変数 ZCODE_APP_VERSION が手動設定より優先されます。",
        "출석과 혜택 조회에 사용되며 클라이언트 버전은 Start Plan 추론 버전에 영향을 주지 않습니다. 중국판과 국제판이 공유하며 환경 변수 ZCODE_APP_VERSION이 수동 설정보다 우선합니다.",
        "Usados para check-in e descoberta de ofertas; a versão do cliente não altera a versão de inferência Start Plan. Compartilhados pelas edições China e Global; ZCODE_APP_VERSION tem prioridade sobre as configurações manuais.",
        "簽到開關與活動查詢使用；用戶端版本號不影響方案池推理使用的版本。國內版與國際版共用這兩項；環境變數 ZCODE_APP_VERSION 優先於手動設定。",
    ]);
    schema
}

pub fn overrides() -> Value {
    std::env::var("ZCODE_APP_VERSION").ok().map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()).map(|value| json!({ claim::SETTING_APP_VERSION: {
            "source": "ZCODE_APP_VERSION", "value": value,
        } })).unwrap_or_else(|| json!({}))
}
