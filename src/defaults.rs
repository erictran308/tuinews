//! The feeds a new tuinews starts with: Vietnam's biggest news sites, each
//! site's newest articles, then two in English; and categories made of the
//! same sites' sections (their Xe, Công nghệ… feeds). All checked to answer
//! with RSS in October 2026 (`cargo test -- --ignored live_every_default`).
//! `a` adds others, `x` on one in the sidebar removes it, `F` puts a feed in
//! categories.

pub struct DefaultFeed {
    pub name: &'static str,
    pub url: &'static str,
}

pub const FEEDS: [DefaultFeed; 15] = [
    DefaultFeed {
        name: "VnExpress",
        url: "https://vnexpress.net/rss/tin-moi-nhat.rss",
    },
    DefaultFeed {
        name: "Tuổi Trẻ",
        url: "https://tuoitre.vn/rss/tin-moi-nhat.rss",
    },
    DefaultFeed {
        name: "Thanh Niên",
        url: "https://thanhnien.vn/rss/home.rss",
    },
    DefaultFeed {
        name: "Dân Trí",
        url: "https://dantri.com.vn/rss/home.rss",
    },
    DefaultFeed {
        name: "VietnamNet",
        url: "https://vietnamnet.vn/rss/thoi-su.rss",
    },
    DefaultFeed {
        name: "VTC News",
        url: "https://vtcnews.vn/rss/trang-chu.rss",
    },
    DefaultFeed {
        name: "Tiền Phong",
        url: "https://tienphong.vn/rss/home.rss",
    },
    DefaultFeed {
        name: "VietnamPlus",
        url: "https://www.vietnamplus.vn/rss/home.rss",
    },
    DefaultFeed {
        name: "Nhân Dân",
        url: "https://nhandan.vn/rss/home.rss",
    },
    DefaultFeed {
        name: "Sài Gòn Giải Phóng",
        url: "https://www.sggp.org.vn/rss/home.rss",
    },
    DefaultFeed {
        name: "CafeF",
        url: "https://cafef.vn/home.rss",
    },
    DefaultFeed {
        name: "GenK",
        url: "https://genk.vn/rss/home.rss",
    },
    DefaultFeed {
        name: "Kenh14",
        url: "https://kenh14.vn/rss/home.rss",
    },
    DefaultFeed {
        name: "VnExpress International",
        url: "https://e.vnexpress.net/rss/news.rss",
    },
    DefaultFeed {
        name: "Tuoi Tre News",
        url: "https://tuoitrenews.vn/rss/home.rss",
    },
];

const fn feed(name: &'static str, url: &'static str) -> DefaultFeed {
    DefaultFeed { name, url }
}

/// A category: its sections of the sites, which show only in it, and the
/// feeds of [`FEEDS`] that are all about it (GenK is all tech).
pub struct DefaultCategory {
    pub name: &'static str,
    pub feeds: &'static [DefaultFeed],
    pub also: &'static [&'static str],
}

pub const CATEGORIES: [DefaultCategory; 8] = [
    DefaultCategory {
        name: "Tech",
        feeds: &[
            feed(
                "VnExpress",
                "https://vnexpress.net/rss/khoa-hoc-cong-nghe.rss",
            ),
            feed("Tuổi Trẻ", "https://tuoitre.vn/rss/cong-nghe.rss"),
            feed("Thanh Niên", "https://thanhnien.vn/rss/cong-nghe.rss"),
            feed("Dân Trí", "https://dantri.com.vn/rss/suc-manh-so.rss"),
            feed(
                "VietnamNet",
                "https://vietnamnet.vn/rss/thong-tin-truyen-thong.rss",
            ),
            feed("VTC News", "https://vtcnews.vn/rss/khoa-hoc-cong-nghe.rss"),
            feed("Tiền Phong", "https://tienphong.vn/rss/cong-nghe.rss"),
        ],
        also: &["https://genk.vn/rss/home.rss"],
    },
    DefaultCategory {
        name: "Cars",
        feeds: &[
            feed("VnExpress", "https://vnexpress.net/rss/oto-xe-may.rss"),
            feed("Tuổi Trẻ", "https://tuoitre.vn/rss/xe.rss"),
            feed("Thanh Niên", "https://thanhnien.vn/rss/xe.rss"),
            feed("Dân Trí", "https://dantri.com.vn/rss/o-to-xe-may.rss"),
            feed("VietnamNet", "https://vietnamnet.vn/rss/oto-xe-may.rss"),
            feed("VTC News", "https://vtcnews.vn/rss/oto-xe-may.rss"),
            feed("Tiền Phong", "https://tienphong.vn/rss/xe.rss"),
        ],
        also: &[],
    },
    DefaultCategory {
        name: "Business",
        feeds: &[
            feed("VnExpress", "https://vnexpress.net/rss/kinh-doanh.rss"),
            feed("Tuổi Trẻ", "https://tuoitre.vn/rss/kinh-doanh.rss"),
            feed("Thanh Niên", "https://thanhnien.vn/rss/kinh-te.rss"),
            feed("Dân Trí", "https://dantri.com.vn/rss/kinh-doanh.rss"),
            feed("VietnamNet", "https://vietnamnet.vn/kinh-doanh.rss"),
            feed("VTC News", "https://vtcnews.vn/rss/kinh-te.rss"),
            feed("Tiền Phong", "https://tienphong.vn/rss/kinh-te.rss"),
        ],
        also: &["https://cafef.vn/home.rss"],
    },
    DefaultCategory {
        name: "World",
        feeds: &[
            feed("VnExpress", "https://vnexpress.net/rss/the-gioi.rss"),
            feed("Tuổi Trẻ", "https://tuoitre.vn/rss/the-gioi.rss"),
            feed("Thanh Niên", "https://thanhnien.vn/rss/the-gioi.rss"),
            feed("Dân Trí", "https://dantri.com.vn/rss/the-gioi.rss"),
            feed("VietnamNet", "https://vietnamnet.vn/rss/the-gioi.rss"),
            feed("VTC News", "https://vtcnews.vn/rss/the-gioi.rss"),
        ],
        also: &[],
    },
    DefaultCategory {
        name: "Sports",
        feeds: &[
            feed("VnExpress", "https://vnexpress.net/rss/the-thao.rss"),
            feed("Tuổi Trẻ", "https://tuoitre.vn/rss/the-thao.rss"),
            feed("Thanh Niên", "https://thanhnien.vn/rss/the-thao.rss"),
            feed("Dân Trí", "https://dantri.com.vn/rss/the-thao.rss"),
            feed("VietnamNet", "https://vietnamnet.vn/rss/the-thao.rss"),
            feed("VTC News", "https://vtcnews.vn/rss/the-thao.rss"),
        ],
        also: &[],
    },
    DefaultCategory {
        name: "Entertainment",
        feeds: &[
            feed("VnExpress", "https://vnexpress.net/rss/giai-tri.rss"),
            feed("Tuổi Trẻ", "https://tuoitre.vn/rss/giai-tri.rss"),
            feed("Thanh Niên", "https://thanhnien.vn/rss/giai-tri.rss"),
            feed("Dân Trí", "https://dantri.com.vn/rss/giai-tri.rss"),
            feed("VietnamNet", "https://vietnamnet.vn/rss/giai-tri.rss"),
        ],
        also: &["https://kenh14.vn/rss/home.rss"],
    },
    DefaultCategory {
        name: "Health",
        feeds: &[
            feed("VnExpress", "https://vnexpress.net/rss/suc-khoe.rss"),
            feed("Tuổi Trẻ", "https://tuoitre.vn/rss/suc-khoe.rss"),
            feed("Thanh Niên", "https://thanhnien.vn/rss/suc-khoe.rss"),
            feed("Dân Trí", "https://dantri.com.vn/rss/suc-khoe.rss"),
            feed("VietnamNet", "https://vietnamnet.vn/suc-khoe.rss"),
        ],
        also: &[],
    },
    DefaultCategory {
        name: "Travel",
        feeds: &[
            feed("VnExpress", "https://vnexpress.net/rss/du-lich.rss"),
            feed("Tuổi Trẻ", "https://tuoitre.vn/rss/du-lich.rss"),
            feed("Thanh Niên", "https://thanhnien.vn/rss/du-lich.rss"),
            feed("Dân Trí", "https://dantri.com.vn/rss/du-lich.rss"),
        ],
        also: &[],
    },
];
