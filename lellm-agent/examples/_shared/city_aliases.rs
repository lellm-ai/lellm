//! 热门城市别名表 — weather_agent 专用。
//!
//! 地址 → wttr.in 城市英文名映射。
//! 查询 O(1)，耗时 <1ms。
//! wttr.in 城市名使用 kebab-case，如 `new-york`、`tai-bei`。

use std::collections::HashMap;
use std::sync::LazyLock;

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
