#!/usr/bin/env python3
"""Offline oracle negatives; real X events remain CI-only acceptance."""
import copy
import unittest
from fixtures.dock_configure_check import verify


def valid():
    event=dict(send_event=True,event=42,window=42,geometry=[5,5,1270,42,0],above=0,override_redirect=False)
    case=dict(events=[event],synthetic_count=1,real_count=0,server_geometry_unchanged=True,server_geometry=event["geometry"],passed=True)
    return dict(window=42,geometry=event['geometry'],cases=[dict(copy.deepcopy(case),name=name) for name in ("rejected_resize","same_geometry","rejected_position","rejected_border","stack_only")],passed=True)


class OracleTests(unittest.TestCase):
    def test_accepts_authoritative_once(self):
        verify(valid())

    def test_does_not_assume_number_of_real_events(self):
        result=valid()
        for case in result['cases']:
            real=dict(case['events'][0],send_event=False)
            case['events'] += [real]*7
            case['real_count']=7
        verify(result)

    def test_rejects_missing_duplicate_wrong_ack_and_geometry(self):
        for mutation in ('missing','duplicate','geometry','window','border','above','override','server'):
            with self.subTest(mutation=mutation):
                result=valid(); case=result['cases'][0]; event=case['events'][0]
                if mutation=='missing':case['events']=[];case['synthetic_count']=0
                elif mutation=='duplicate':case['events']*=2;case['synthetic_count']=2
                elif mutation=='geometry':event['geometry']=[0,0,1280,42,0]
                elif mutation=='window':event['event']=43
                elif mutation=='border':event['geometry']=[5,5,1270,42,2]
                elif mutation=='above':event['above']=43
                elif mutation=='override':event['override_redirect']=True
                else:case['server_geometry_unchanged']=False
                with self.assertRaises(AssertionError):verify(result)


if __name__=='__main__':unittest.main()
