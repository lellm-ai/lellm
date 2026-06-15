//! 城市数据表 — 别名映射 + 拼音映射。
//!
//! 被 `city_resolver` 模块消费。

use std::collections::HashMap;
use std::sync::LazyLock;

/// 第一级：本地别名表（O(1) 查找）。
///
/// 常见地名 → wttr.in 城市名（kebab-case）。
pub static CITY_ALIASES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    macro_rules! insert {
        ($city:literal; $($alias:literal),+ $(,)?) => {
            $( m.insert($alias, $city); )*
        };
    }

    insert!("shanghai"; "上海", "浦东", "陆家嘴", "外滩", "静安", "南京路", "虹桥", "松江", "闵行", "徐汇", "黄浦", "潍坊路");
    insert!("beijing"; "北京", "朝阳", "国贸", "三里屯", "中关村", "西单", "王府井", "海淀", "望京", "亦庄");
    insert!("guangzhou"; "广州", "天河", "珠江新城", "白云", "越秀", "海珠", "琶洲", "北京路");
    insert!("shenzhen"; "深圳", "南山", "福田", "罗湖", "宝安", "龙华", "前海", "科技园", "华强北");
    insert!("hangzhou"; "杭州", "西湖", "滨江", "余杭", "未来科技城", "钱江新城", "武林广场");
    insert!("chengdu"; "成都", "春熙路", "太古里", "宽窄巷子", "锦里", "武侯", "高新", "玉林");
    insert!("chongqing"; "重庆", "解放碑", "洪崖洞", "江北嘴", "观音桥", "渝中");
    insert!("wuhan"; "武汉", "光谷", "江汉路", "黄鹤楼", "武昌", "汉口", "东湖");
    insert!("nanjing"; "南京", "新街口", "夫子庙", "玄武湖", "中山陵", "河西", "秦淮");
    insert!("xian"; "西安", "钟楼", "大雁塔", "回民街", "城墙", "高新", "曲江");
    insert!("qingdao"; "青岛", "栈桥", "八大关", "五四广场", "崂山");
    insert!("dalian"; "大连", "星海广场", "中山广场", "星海湾");
    insert!("xiamen"; "厦门", "鼓浪屿", "曾厝垵", "环岛路", "中山路");
    insert!("changsha"; "长沙", "五一广场", "橘子洲", "岳麓山", "天心阁");
    insert!("harbin"; "哈尔滨", "中央大街", "索菲亚教堂", "太阳岛", "冰雪大世界");
    insert!("kunming"; "昆明", "滇池", "翠湖", "石林");
    insert!("sanya"; "三亚", "亚龙湾", "海棠湾", "大东海", "天涯海角");
    insert!("lasa"; "拉萨", "布达拉宫", "大昭寺", "八廓街");
    insert!("tai-pei"; "台北", "101", "西门町", "台北101", "士林夜市");
    insert!("hong-kong"; "香港", "旺角", "铜锣湾", "中环", "尖沙咀", "维港");
    insert!("macau"; "澳门", "大三巴", "威尼斯人", "新葡京");
    insert!("tokyo"; "东京", "东京都", "新宿", "涩谷", "浅草", "秋叶原", "池袋", "银座", "上野", "横滨");
    insert!("osaka"; "大阪", "心斋桥", "道顿堀", "难波", "梅田");
    insert!("seoul"; "首尔", "明洞", "弘大", "江南", "景福宫");
    insert!("new-york"; "纽约", "曼哈顿", "布鲁克林", "时代广场", "中央公园", "华尔街", "第五大道");
    insert!("london"; "伦敦", "皮卡迪利", "牛津街", "塔桥", "大本钟", "西区");
    insert!("paris"; "巴黎", "埃菲尔", "香榭丽舍", "凯旋门", "卢浮宫");
    insert!("singapore"; "新加坡", "滨海湾", "鱼尾狮", "乌节路");
    insert!("bangkok"; "曼谷", "大皇宫", "暹罗", "苏梅");
    insert!("sydney"; "悉尼", "歌剧院", "海港大桥", "邦迪海滩");

    m
});

/// 中文地名 → 拼音 kebab-case 静态映射。
///
/// 覆盖自治区、自治州、地级市、常见县/县级市。
/// 前缀匹配使用 `max_by_key` 取最长命中，不依赖排列顺序。
pub const CITY_PINYIN_MAP: &[(&str, &str)] = &[
    // — 自治区 —
    ("新疆维吾尔自治区", "xinjiang"),
    ("西藏自治区", "xizang"),
    ("内蒙古自治区", "neimenggu"),
    ("广西壮族自治区", "guangxi"),
    ("宁夏回族自治区", "ningxia"),
    // — 新疆 —
    ("巴音郭楞蒙古自治州", "bayinguoleng"),
    ("克孜勒苏柯尔克孜自治州", "kezilesu"),
    ("伊犁哈萨克自治州", "yili"),
    ("博尔塔拉蒙古自治州", "boertala"),
    ("昌吉回族自治州", "changji"),
    ("和田地区", "hetian"),
    ("喀什地区", "kashi"),
    ("阿克苏地区", "akesu"),
    ("塔城地区", "tachen"),
    ("阿勒泰地区", "aletai"),
    ("吐鲁番市", "tulufan"),
    ("哈密市", "hami"),
    ("乌鲁木齐市", "wulumuqi"),
    ("克拉玛依市", "kelamayi"),
    ("奇台县", "qitai"),
    ("巴音郭楞", "bayinguoleng"),
    ("克孜勒苏", "kezilesu"),
    ("博尔塔拉", "boertala"),
    ("昌吉州", "changji"),
    ("昌吉市", "changji"),
    ("哈密", "hami"),
    ("吐鲁番", "tulufan"),
    ("阿勒泰", "aletai"),
    ("塔城", "tachen"),
    ("阿克苏", "akesu"),
    ("喀什", "kashi"),
    ("和田", "hetian"),
    ("伊犁", "yili"),
    ("克拉玛依", "kelamayi"),
    ("乌鲁木齐", "wulumuqi"),
    // — 西藏 —
    ("拉萨市", "lasa"),
    ("日喀则市", "rikaze"),
    ("昌都市", "changdu"),
    ("林芝市", "linzhi"),
    ("山南市", "shannan"),
    ("那曲市", "naqu"),
    ("阿里地区", "ali"),
    ("拉萨", "lasa"),
    ("日喀则", "rikaze"),
    ("昌都", "changdu"),
    ("林芝", "linzhi"),
    ("山南", "shannan"),
    ("那曲", "naqu"),
    ("阿里", "ali"),
    // — 内蒙古 —
    ("呼伦贝尔市", "hulunbeier"),
    ("兴安盟", "xinganmeng"),
    ("锡林郭勒盟", "xilinner"),
    ("阿拉善盟", "alashanmeng"),
    ("通辽市", "tongliao"),
    ("赤峰市", "chifeng"),
    ("乌兰察布市", "ulanqab"),
    ("鄂尔多斯市", "eerduosi"),
    ("巴彦淖尔市", "bayannur"),
    ("乌海市", "wuhai"),
    ("包头市", "baotou"),
    ("呼和浩特市", "huhehaote"),
    ("呼伦贝尔", "hulunbeier"),
    ("兴安盟", "xinganmeng"),
    ("锡林郭勒", "xilinner"),
    ("阿拉善", "alashanmeng"),
    ("通辽", "tongliao"),
    ("赤峰", "chifeng"),
    ("乌兰察布", "ulanqab"),
    ("鄂尔多斯", "eerduosi"),
    ("巴彦淖尔", "bayannur"),
    ("乌海", "wuhai"),
    ("包头", "baotou"),
    ("呼和浩特", "huhehaote"),
    // — 广西 —
    ("南宁市", "nanning"),
    ("柳州市", "liuzhou"),
    ("桂林市", "guilin"),
    ("梧州市", "wuzhou"),
    ("北海市", "beihai"),
    ("崇左市", "chongzuo"),
    ("来宾市", "laibin"),
    ("河池市", "hechi"),
    ("百色市", "baise"),
    ("贺州市", "hezhou"),
    ("玉林市", "yulin"),
    ("防城港市", "fangchenggang"),
    ("钦州市", "qinzhou"),
    ("贵港市", "guigang"),
    ("南宁", "nanning"),
    ("柳州", "liuzhou"),
    ("桂林", "guilin"),
    ("梧州", "wuzhou"),
    ("北海", "beihai"),
    ("崇左", "chongzuo"),
    ("来宾", "laibin"),
    ("河池", "hechi"),
    ("百色", "baise"),
    ("贺州", "hezhou"),
    ("玉林", "yulin"),
    ("防城港", "fangchenggang"),
    ("钦州", "qinzhou"),
    ("贵港", "guigang"),
    // — 宁夏 —
    ("银川市", "yinchuan"),
    ("石嘴山市", "shizuishan"),
    ("吴忠市", "wuzhong"),
    ("固原市", "guyuan"),
    ("中卫市", "zhongwei"),
    ("银川", "yinchuan"),
    ("石嘴山", "shizuishan"),
    ("吴忠", "wuzhong"),
    ("固原", "guyuan"),
    ("中卫", "zhongwei"),
    // — 青海 —
    ("西宁市", "xining"),
    ("海东市", "haidong"),
    ("海北州", "haibei"),
    ("黄南州", "huangnan"),
    ("海南州", "hainan"),
    ("果洛州", "guoluo"),
    ("玉树州", "yushu"),
    ("海西州", "haixi"),
    ("西宁", "xining"),
    ("海东", "haidong"),
    ("海北", "haibei"),
    ("黄南", "huangnan"),
    ("果洛", "guoluo"),
    ("玉树", "yushu"),
    ("海西", "haixi"),
    // — 甘肃 —
    ("兰州市", "lanzhou"),
    ("天水市", "tianshui"),
    ("嘉峪关市", "jiayuguan"),
    ("金昌市", "jinchang"),
    ("白银市", "baiyin"),
    ("武威市", "wuwei"),
    ("张掖市", "zhangye"),
    ("平凉市", "pingliang"),
    ("酒泉市", "jiuquan"),
    ("庆阳市", "qingyang"),
    ("定西市", "dingxi"),
    ("陇南市", "longnan"),
    ("临夏州", "linxia"),
    ("甘南州", "gannan"),
    ("兰州", "lanzhou"),
    ("天水", "tianshui"),
    ("嘉峪关", "jiayuguan"),
    ("金昌", "jinchang"),
    ("白银", "baiyin"),
    ("武威", "wuwei"),
    ("张掖", "zhangye"),
    ("平凉", "pingliang"),
    ("酒泉", "jiuquan"),
    ("庆阳", "qingyang"),
    ("定西", "dingxi"),
    ("陇南", "longnan"),
    ("临夏", "linxia"),
    ("甘南", "gannan"),
    // — 贵州 —
    ("贵阳市", "guiyang"),
    ("六盘水市", "liupanshui"),
    ("遵义市", "zunyi"),
    ("安顺市", "anshun"),
    ("毕节市", "bijie"),
    ("铜仁市", "tongren"),
    ("黔西南州", "qianxinan"),
    ("黔东南州", "qiandongnan"),
    ("黔南州", "qiannan"),
    ("贵阳", "guiyang"),
    ("六盘水", "liupanshui"),
    ("遵义", "zunyi"),
    ("安顺", "anshun"),
    ("毕节", "bijie"),
    ("铜仁", "tongren"),
    ("黔西南", "qianxinan"),
    ("黔东南", "qiandongnan"),
    ("黔南", "qiannan"),
    // — 云南 —
    ("昆明市", "kunming"),
    ("曲靖市", "qujing"),
    ("玉溪市", "yuxi"),
    ("保山市", "baoshan"),
    ("昭通市", "zhaotong"),
    ("丽江市", "lijiang"),
    ("普洱市", "puer"),
    ("临沧市", "lincang"),
    ("楚雄州", "chuxiong"),
    ("红河州", "honghe"),
    ("文山州", "wenshan"),
    ("西双版纳州", "xishuangbanna"),
    ("大理州", "dali"),
    ("德宏州", "dehong"),
    ("怒江州", "nujiang"),
    ("迪庆州", "diqing"),
    ("昆明", "kunming"),
    ("曲靖", "qujing"),
    ("玉溪", "yuxi"),
    ("保山", "baoshan"),
    ("昭通", "zhaotong"),
    ("丽江", "lijiang"),
    ("普洱", "puer"),
    ("临沧", "lincang"),
    ("楚雄", "chuxiong"),
    ("红河", "honghe"),
    ("文山", "wenshan"),
    ("西双版纳", "xishuangbanna"),
    ("大理", "dali"),
    ("德宏", "dehong"),
    ("怒江", "nujiang"),
    ("迪庆", "diqing"),
    // — 四川 —
    ("成都市", "chengdu"),
    ("绵阳市", "mianyang"),
    ("泸州市", "luzhou"),
    ("德阳市", "deyang"),
    ("广元市", "guangyuan"),
    ("遂宁市", "suining"),
    ("内江市", "neijiang"),
    ("乐山市", "leshan"),
    ("南充市", "nanchong"),
    ("宜宾市", "yibin"),
    ("广安市", "guangan"),
    ("达州市", "dazhou"),
    ("巴中市", "bazhong"),
    ("雅安市", "yaan"),
    ("眉山市", "meishan"),
    ("资阳市", "ziyang"),
    ("阿坝州", "aba"),
    ("甘孜州", "ganzi"),
    ("凉山州", "liangshan"),
    ("成都", "chengdu"),
    ("绵阳", "mianyang"),
    ("泸州", "luzhou"),
    ("德阳", "deyang"),
    ("广元", "guangyuan"),
    ("遂宁", "suining"),
    ("内江", "neijiang"),
    ("乐山", "leshan"),
    ("南充", "nanchong"),
    ("宜宾", "yibin"),
    ("广安", "guangan"),
    ("达州", "dazhou"),
    ("巴中", "bazhong"),
    ("雅安", "yaan"),
    ("眉山", "meishan"),
    ("资阳", "ziyang"),
    ("阿坝", "aba"),
    ("甘孜", "ganzi"),
    ("凉山", "liangshan"),
    // — 重庆 —
    ("重庆市", "chongqing"),
    ("重庆", "chongqing"),
    // — 北京 —
    ("北京市", "beijing"),
    ("北京", "beijing"),
    // — 天津 —
    ("天津市", "tianjin"),
    ("天津", "tianjin"),
    // — 上海 —
    ("上海市", "shanghai"),
    ("上海", "shanghai"),
];
