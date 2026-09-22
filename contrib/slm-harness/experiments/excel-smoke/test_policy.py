import unittest
from policy import validate_request

POLICY={'sheet':'Data','range':'A1:D9','allow_output_sheets':['Summary'],'formula_targets':['F2'],'allow_mutation':True}
def request(op='deduplicate',params=None,**extra):
    return dict(operation=op,sheet='Data',range='F2' if op=='set_formula' else 'A1:D9',expected_revision=0,parameters=params or {'keys':['订单号']},**extra)
class PolicyTests(unittest.TestCase):
    def test_valid_request_is_accepted(self):
        result=validate_request(request(),POLICY,0)
        self.assertIsInstance(result,dict)
        self.assertEqual(result['operation'],'deduplicate')
    def test_escape_target_denied(self):
        req=request();req['range']='A1:XFD1048576'
        self.assertRaises(ValueError,validate_request,req,POLICY,0)
    def test_stale_revision_denied(self):
        self.assertRaises(ValueError,validate_request,request(),POLICY,1)
    def test_outside_sheet_denied(self):
        req=request();req['sheet']='Protected'
        self.assertRaises(ValueError,validate_request,req,POLICY,0)
    def test_unknown_function_denied(self):
        req=request('set_formula',{'function':'WEBSERVICE','source_range':'D2:D9','destination':'F2'})
        self.assertRaises(ValueError,validate_request,req,POLICY,0)
    def test_formula_source_escape_denied(self):
        req=request('set_formula',{'function':'SUM','source_range':'D2:D99','destination':'F2'})
        self.assertRaises(ValueError,validate_request,req,POLICY,0)
    def test_formula_destination_escape_denied(self):
        req=request('set_formula',{'function':'SUM','source_range':'D2:D9','destination':'A1'})
        self.assertRaises(ValueError,validate_request,req,POLICY,0)
    def test_unknown_parameters_denied(self):
        self.assertRaises(ValueError,validate_request,request(params={'keys':['订单号'],'script':'anything'}),POLICY,0)
    def test_readonly_policy_denied(self):
        self.assertRaises(ValueError,validate_request,request(),dict(POLICY,allow_mutation=False),0)
    def test_sum_formula_accepted(self):
        req=request('set_formula',{'function':'SUM','source_range':'D2:D9','destination':'F2'})
        self.assertEqual(validate_request(req,POLICY,0),req)
    def test_formula_target_must_match_envelope(self):
        req=request('set_formula',{'function':'SUM','source_range':'D2:D9','destination':'F2'});req['range']='A1:D9'
        self.assertRaises(ValueError,validate_request,req,POLICY,0)
if __name__=='__main__':unittest.main()
