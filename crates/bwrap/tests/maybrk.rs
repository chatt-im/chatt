use bwrap::Result;
use bwrap::WrapStyle;
use bwrap::Wrapper;

mod ascii {
    use super::*;

    #[test]
    fn _1() -> Result<()> {
        let before = "hello";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 3, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], b"hel\nlo");

        Ok(())
    }
    #[test]
    fn _2() -> Result<()> {
        let before = "hello world";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 4, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], b"hell\no wo\nrld");

        Ok(())
    }
    #[test]
    fn _3() -> Result<()> {
        let before = "hello hello hello";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 4, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], b"hell\no he\nllo \nhell\no");

        Ok(())
    }
    // -
}

mod ascii_existnl {
    use super::*;

    #[test]
    fn _1() -> Result<()> {
        let before = "hel\nlo";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 3, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "hel\nlo".as_bytes());

        Ok(())
    }
    #[test]
    fn _2() -> Result<()> {
        let before = "hel\nlo \nworld";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 3, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "hel\nlo \nwor\nld".as_bytes());

        Ok(())
    }
    #[test]
    fn _3() -> Result<()> {
        let before = "hel\nlo \nwor\nld";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 3, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "hel\nlo \nwor\nld".as_bytes());

        Ok(())
    }
    #[test]
    fn _4() -> Result<()> {
        let before = "\nhell\no";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 3, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "\nhel\nl\no".as_bytes());

        Ok(())
    }
    #[test]
    fn _5() -> Result<()> {
        let before = "\nhhhhh\nhhhhh\n";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 3, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "\nhhh\nhh\nhhh\nhh\n".as_bytes());

        Ok(())
    }
    #[test]
    fn _6() -> Result<()> {
        let before = "\nh\nh\nh\nh\nh\nh\nhhhh\n";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 3, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "\nh\nh\nh\nh\nh\nh\nhhh\nh\n".as_bytes());

        Ok(())
    }
    #[test]
    fn _7() -> Result<()> {
        let before = "\n\n\n\n\nhhhhh\n\n\n\n\nhhhhh\n\n\n\n\n";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 3, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "\n\n\n\n\nhhh\nhh\n\n\n\n\nhhh\nhh\n\n\n\n\n".as_bytes()
        );

        Ok(())
    }
    #[test]
    fn _8() -> Result<()> {
        let before = "\nh\nh\nh\nh\nh\nh\nh\nh\nh\nh\n";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 1, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "\nh\nh\nh\nh\nh\nh\nh\nh\nh\nh\n".as_bytes());

        Ok(())
    }
    // -
}

mod nonascii {
    use super::*;

    #[test]
    fn _1() -> Result<()> {
        let before = "ＨＨＨＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "ＨＨＨ\nＨＨ".as_bytes());

        Ok(())
    }

    #[test]
    fn _2() -> Result<()> {
        let before = "ＨＨＨＨＨ ＨＨＨＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "ＨＨＨ\nＨＨ Ｈ\nＨＨＨ\nＨ".as_bytes());

        Ok(())
    }

    #[test]
    fn _3() -> Result<()> {
        let before = "ＨＨＨＨＨ ＨＨＨＨＨ ＨＨＨＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "ＨＨＨ\nＨＨ Ｈ\nＨＨＨ\nＨ ＨＨ\nＨＨＨ".as_bytes()
        );

        Ok(())
    }

    #[test]
    fn _4() -> Result<()> {
        let before = "ＨＨＨＨＨ ＨＨＨＨＨ ＨＨＨＨＨ ＨＨＨＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "ＨＨＨ\nＨＨ Ｈ\nＨＨＨ\nＨ ＨＨ\nＨＨＨ \nＨＨＨ\nＨＨ".as_bytes()
        );

        Ok(())
    }

    #[test]
    fn _5() -> Result<()> {
        let before = "ＨＨＨＨＨ ＨＨＨhＨ ＨＨＨＨＨ ＨＨＨＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "ＨＨＨ\nＨＨ Ｈ\nＨＨhＨ\n ＨＨＨ\nＨＨ Ｈ\nＨＨＨ\nＨ".as_bytes()
        );

        Ok(())
    }
}

mod nonascii_existnl {
    use super::*;

    #[test]
    fn _1() -> Result<()> {
        let before = "ＨＨＨ\nＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "ＨＨＨ\nＨＨ".as_bytes());

        Ok(())
    }
    #[test]
    fn _2() -> Result<()> {
        let before = "ＨＨＨ\nＨＨ ＨＨＨＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(&after[..len], "ＨＨＨ\nＨＨ Ｈ\nＨＨＨ\nＨ".as_bytes());

        Ok(())
    }
    #[test]
    fn _3() -> Result<()> {
        let before = "ＨＨＨ\nＨＨ Ｈ\nＨＨＨＨ ＨＨ\nＨＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "ＨＨＨ\nＨＨ Ｈ\nＨＨＨ\nＨ ＨＨ\nＨＨＨ".as_bytes()
        );

        Ok(())
    }
    #[test]
    fn _4() -> Result<()> {
        let before = "ＨＨＨ\n\n\nＨＨ Ｈ\n\n\nＨＨＨＨ ＨＨ\n\n\nＨＨＨ";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "ＨＨＨ\n\n\nＨＨ Ｈ\n\n\nＨＨＨ\nＨ ＨＨ\n\n\nＨＨＨ".as_bytes()
        );

        Ok(())
    }
    #[test]
    fn _5() -> Result<()> {
        let before = "\n\n\nＨＨＨ\n\n\nＨＨ Ｈ\n\n\nＨＨＨＨ ＨＨ\n\n\nＨＨＨ\n\n\n";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "\n\n\nＨＨＨ\n\n\nＨＨ Ｈ\n\n\nＨＨＨ\nＨ ＨＨ\n\n\nＨＨＨ\n\n\n".as_bytes()
        );

        Ok(())
    }
    #[test]
    fn _6() -> Result<()> {
        // similar to _5, but with one ascii
        let before = "\n\n\nＨＨＨ\n\n\nＨＨ Ｈ\n\n\nＨＨＨh ＨＨ\n\n\nＨＨＨ\n\n\n";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 7, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "\n\n\nＨＨＨ\n\n\nＨＨ Ｈ\n\n\nＨＨＨh\n ＨＨ\n\n\nＨＨＨ\n\n\n".as_bytes()
        );

        Ok(())
    }
    #[test]
    fn _7() -> Result<()> {
        // note, compared to ascii_existnl::_8, similar input but
        //       very different output. As for this one, max_width
        //       is inside a unicode code point, hence NL will
        //       be inserted anyway.
        //
        let before = "\nＨ\nＨ\nＨ\nＨ\nＨ\nＨ\nＨ\nＨ\nＨ\nＨ\n";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 1, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;
        assert_eq!(
            &after[..len],
            "\n\nＨ\n\nＨ\n\nＨ\n\nＨ\n\nＨ\n\nＨ\n\nＨ\n\nＨ\n\nＨ\n\nＨ\n".as_bytes()
        );
        Ok(())
    }
    //-
}

mod unicode_sequences {
    use super::*;

    #[test]
    fn combining_mark_stays_with_base() -> Result<()> {
        let before = "e\u{301}x";
        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 1, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;

        assert_eq!(&after[..len], "e\u{301}\nx".as_bytes());

        Ok(())
    }

    #[test]
    fn emoji_zwj_sequence_stays_together() -> Result<()> {
        let before = "👩‍💻x";
        assert_eq!(unicode_width::UnicodeWidthStr::width("👩‍💻"), 2);

        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 2, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;

        assert_eq!(&after[..len], "👩‍💻\nx".as_bytes());

        Ok(())
    }

    #[test]
    fn text_presentation_selector_can_reduce_width() -> Result<()> {
        let text_emoji = "☔\u{FE0E}";
        let before = "a☔\u{FE0E}x";
        assert_eq!(unicode_width::UnicodeWidthStr::width(text_emoji), 1);

        let mut after = [0u8; 256];
        let len =
            Wrapper::new(before, 2, &mut after)?.wrap_use_style(WrapStyle::MayBrk(None, None))?;

        assert_eq!(&after[..len], "a☔\u{FE0E}\nx".as_bytes());

        Ok(())
    }
}
