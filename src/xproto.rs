#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#![allow(unused)]
#![allow(unused_mut)]

pub const X_CreateWindow: u8 = 1;
pub const X_ChangeWindowAttributes: u8 = 2;
pub const X_GetWindowAttributes: u8 = 3;
pub const X_DestroyWindow: u8 = 4;
pub const X_DestroySubwindows: u8 = 5;
pub const X_ChangeSaveSet: u8 = 6;
pub const X_ReparentWindow: u8 = 7;
pub const X_MapWindow: u8 = 8;
pub const X_MapSubwindows: u8 = 9;
pub const X_UnmapWindow: u8 = 10;
pub const X_UnmapSubwindows: u8 = 11;
pub const X_ConfigureWindow: u8 = 12;
pub const X_CirculateWindow: u8 = 13;
pub const X_GetGeometry: u8 = 14;
pub const X_QueryTree: u8 = 15;
pub const X_InternAtom: u8 = 16;
pub const X_GetAtomName: u8 = 17;
pub const X_ChangeProperty: u8 = 18;
pub const X_DeleteProperty: u8 = 19;
pub const X_GetProperty: u8 = 20;
pub const X_ListProperties: u8 = 21;
pub const X_SetSelectionOwner: u8 = 22;
pub const X_GetSelectionOwner: u8 = 23;
pub const X_ConvertSelection: u8 = 24;
pub const X_SendEvent: u8 = 25;
pub const X_GrabPointer: u8 = 26;
pub const X_UngrabPointer: u8 = 27;
pub const X_GrabButton: u8 = 28;
pub const X_UngrabButton: u8 = 29;
pub const X_ChangeActivePointerGrab: u8 = 30;
pub const X_GrabKeyboard: u8 = 31;
pub const X_UngrabKeyboard: u8 = 32;
pub const X_GrabKey: u8 = 33;
pub const X_UngrabKey: u8 = 34;
pub const X_AllowEvents: u8 = 35;
pub const X_GrabServer: u8 = 36;
pub const X_UngrabServer: u8 = 37;
pub const X_QueryPointer: u8 = 38;
pub const X_GetMotionEvents: u8 = 39;
pub const X_TranslateCoords: u8 = 40;
pub const X_WarpPointer: u8 = 41;
pub const X_SetInputFocus: u8 = 42;
pub const X_GetInputFocus: u8 = 43;
pub const X_QueryKeymap: u8 = 44;
pub const X_OpenFont: u8 = 45;
pub const X_CloseFont: u8 = 46;
pub const X_QueryFont: u8 = 47;
pub const X_QueryTextExtents: u8 = 48;
pub const X_ListFonts: u8 = 49;
pub const X_ListFontsWithInfo: u8 = 50;
pub const X_SetFontPath: u8 = 51;
pub const X_GetFontPath: u8 = 52;
pub const X_CreatePixmap: u8 = 53;
pub const X_FreePixmap: u8 = 54;
pub const X_CreateGC: u8 = 55;
pub const X_ChangeGC: u8 = 56;
pub const X_CopyGC: u8 = 57;
pub const X_SetDashes: u8 = 58;
pub const X_SetClipRectangles: u8 = 59;
pub const X_FreeGC: u8 = 60;
pub const X_ClearArea: u8 = 61;
pub const X_CopyArea: u8 = 62;
pub const X_CopyPlane: u8 = 63;
pub const X_PolyPoint: u8 = 64;
pub const X_PolyLine: u8 = 65;
pub const X_PolySegment: u8 = 66;
pub const X_PolyRectangle: u8 = 67;
pub const X_PolyArc: u8 = 68;
pub const X_FillPoly: u8 = 69;
pub const X_PolyFillRectangle: u8 = 70;
pub const X_PolyFillArc: u8 = 71;
pub const X_PutImage: u8 = 72;
pub const X_GetImage: u8 = 73;
pub const X_PolyText8: u8 = 74;
pub const X_PolyText16: u8 = 75;
pub const X_ImageText8: u8 = 76;
pub const X_ImageText16: u8 = 77;
pub const X_CreateColormap: u8 = 78;
pub const X_FreeColormap: u8 = 79;
pub const X_CopyColormapAndFree: u8 = 80;
pub const X_InstallColormap: u8 = 81;
pub const X_UninstallColormap: u8 = 82;
pub const X_ListInstalledColormaps: u8 = 83;
pub const X_AllocColor: u8 = 84;
pub const X_AllocNamedColor: u8 = 85;
pub const X_AllocColorCells: u8 = 86;
pub const X_AllocColorPlanes: u8 = 87;
pub const X_FreeColors: u8 = 88;
pub const X_StoreColors: u8 = 89;
pub const X_StoreNamedColor: u8 = 90;
pub const X_QueryColors: u8 = 91;
pub const X_LookupColor: u8 = 92;
pub const X_CreateCursor: u8 = 93;
pub const X_CreateGlyphCursor: u8 = 94;
pub const X_FreeCursor: u8 = 95;
pub const X_RecolorCursor: u8 = 96;
pub const X_QueryBestSize: u8 = 97;
pub const X_QueryExtension: u8 = 98;
pub const X_ListExtensions: u8 = 99;
pub const X_ChangeKeyboardMapping: u8 = 100;
pub const X_GetKeyboardMapping: u8 = 101;
pub const X_ChangeKeyboardControl: u8 = 102;
pub const X_GetKeyboardControl: u8 = 103;
pub const X_Bell: u8 = 104;
pub const X_ChangePointerControl: u8 = 105;
pub const X_GetPointerControl: u8 = 106;
pub const X_SetScreenSaver: u8 = 107;
pub const X_GetScreenSaver: u8 = 108;
pub const X_ChangeHosts: u8 = 109;
pub const X_ListHosts: u8 = 110;
pub const X_SetAccessControl: u8 = 111;
pub const X_SetCloseDownMode: u8 = 112;
pub const X_KillClient: u8 = 113;
pub const X_RotateProperties: u8 = 114;
pub const X_ForceScreenSaver: u8 = 115;
pub const X_SetPointerMapping: u8 = 116;
pub const X_GetPointerMapping: u8 = 117;
pub const X_SetModifierMapping: u8 = 118;
pub const X_GetModifierMapping: u8 = 119;
pub const X_NoOperation: u8 = 127;

// definitions for initial window state.
pub const WithdrawnState: u8 = 0;
pub const NormalState: u8 = 1;
pub const IconicState: u8 = 2;

pub const XC_num_glyphs: u32 = 154;
pub const XC_X_cursor: u32 = 0;
pub const XC_arrow: u32 = 2;
pub const XC_based_arrow_down: u32 = 4;
pub const XC_based_arrow_up: u32 = 6;
pub const XC_boat: u32 = 8;
pub const XC_bogosity: u32 = 10;
pub const XC_bottom_left_corner: u32 = 12;
pub const XC_bottom_right_corner: u32 = 14;
pub const XC_bottom_side: u32 = 16;
pub const XC_bottom_tee: u32 = 18;
pub const XC_box_spiral: u32 = 20;
pub const XC_center_ptr: u32 = 22;
pub const XC_circle: u32 = 24;
pub const XC_clock: u32 = 26;
pub const XC_coffee_mug: u32 = 28;
pub const XC_cross: u32 = 30;
pub const XC_cross_reverse: u32 = 32;
pub const XC_crosshair: u32 = 34;
pub const XC_diamond_cross: u32 = 36;
pub const XC_dot: u32 = 38;
pub const XC_dotbox: u32 = 40;
pub const XC_double_arrow: u32 = 42;
pub const XC_draft_large: u32 = 44;
pub const XC_draft_small: u32 = 46;
pub const XC_draped_box: u32 = 48;
pub const XC_exchange: u32 = 50;
pub const XC_fleur: u32 = 52;
pub const XC_gobbler: u32 = 54;
pub const XC_gumby: u32 = 56;
pub const XC_hand1: u32 = 58;
pub const XC_hand2: u32 = 60;
pub const XC_heart: u32 = 62;
pub const XC_icon: u32 = 64;
pub const XC_iron_cross: u32 = 66;
pub const XC_left_ptr: u32 = 68;
pub const XC_left_side: u32 = 70;
pub const XC_left_tee: u32 = 72;
pub const XC_leftbutton: u32 = 74;
pub const XC_ll_angle: u32 = 76;
pub const XC_lr_angle: u32 = 78;
pub const XC_man: u32 = 80;
pub const XC_middlebutton: u32 = 82;
pub const XC_mouse: u32 = 84;
pub const XC_pencil: u32 = 86;
pub const XC_pirate: u32 = 88;
pub const XC_plus: u32 = 90;
pub const XC_question_arrow: u32 = 92;
pub const XC_right_ptr: u32 = 94;
pub const XC_right_side: u32 = 96;
pub const XC_right_tee: u32 = 98;
pub const XC_rightbutton: u32 = 100;
pub const XC_rtl_logo: u32 = 102;
pub const XC_sailboat: u32 = 104;
pub const XC_sb_down_arrow: u32 = 106;
pub const XC_sb_h_double_arrow: u32 = 108;
pub const XC_sb_left_arrow: u32 = 110;
pub const XC_sb_right_arrow: u32 = 112;
pub const XC_sb_up_arrow: u32 = 114;
pub const XC_sb_v_double_arrow: u32 = 116;
pub const XC_shuttle: u32 = 118;
pub const XC_sizing: u32 = 120;
pub const XC_spider: u32 = 122;
pub const XC_spraycan: u32 = 124;
pub const XC_star: u32 = 126;
pub const XC_target: u32 = 128;
pub const XC_tcross: u32 = 130;
pub const XC_top_left_arrow: u32 = 132;
pub const XC_top_left_corner: u32 = 134;
pub const XC_top_right_corner: u32 = 136;
pub const XC_top_side: u32 = 138;
pub const XC_top_tee: u32 = 140;
pub const XC_trek: u32 = 142;
pub const XC_ul_angle: u32 = 144;
pub const XC_umbrella: u32 = 146;
pub const XC_ur_angle: u32 = 148;
pub const XC_watch: u32 = 150;
pub const XC_xterm: u32 = 152;
