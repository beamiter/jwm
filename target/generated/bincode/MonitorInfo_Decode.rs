impl < __Context > :: bincode :: Decode < __Context > for MonitorInfo
{
    fn decode < __D : :: bincode :: de :: Decoder < Context = __Context > >
    (decoder : & mut __D) ->core :: result :: Result < Self, :: bincode ::
    error :: DecodeError >
    {
        core :: result :: Result ::
        Ok(Self
        {
            monitor_num : :: bincode :: Decode :: decode(decoder) ?,
            monitor_width : :: bincode :: Decode :: decode(decoder) ?,
            monitor_height : :: bincode :: Decode :: decode(decoder) ?,
            monitor_x : :: bincode :: Decode :: decode(decoder) ?, monitor_y :
            :: bincode :: Decode :: decode(decoder) ?, tag_status_vec : ::
            bincode :: Decode :: decode(decoder) ?, client_name : :: bincode
            :: Decode :: decode(decoder) ?, ltsymbol : :: bincode :: Decode ::
            decode(decoder) ?,
        })
    }
} impl < '__de, __Context > :: bincode :: BorrowDecode < '__de, __Context >
for MonitorInfo
{
    fn borrow_decode < __D : :: bincode :: de :: BorrowDecoder < '__de,
    Context = __Context > > (decoder : & mut __D) ->core :: result :: Result <
    Self, :: bincode :: error :: DecodeError >
    {
        core :: result :: Result ::
        Ok(Self
        {
            monitor_num : :: bincode :: BorrowDecode ::< '_, __Context >::
            borrow_decode(decoder) ?, monitor_width : :: bincode ::
            BorrowDecode ::< '_, __Context >:: borrow_decode(decoder) ?,
            monitor_height : :: bincode :: BorrowDecode ::< '_, __Context >::
            borrow_decode(decoder) ?, monitor_x : :: bincode :: BorrowDecode
            ::< '_, __Context >:: borrow_decode(decoder) ?, monitor_y : ::
            bincode :: BorrowDecode ::< '_, __Context >::
            borrow_decode(decoder) ?, tag_status_vec : :: bincode ::
            BorrowDecode ::< '_, __Context >:: borrow_decode(decoder) ?,
            client_name : :: bincode :: BorrowDecode ::< '_, __Context >::
            borrow_decode(decoder) ?, ltsymbol : :: bincode :: BorrowDecode
            ::< '_, __Context >:: borrow_decode(decoder) ?,
        })
    }
}